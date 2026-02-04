import json
import os
import time
import traceback

import bittensor as bt
import cli_parser
import websocket
from _miner.server import MinerServer
from bittensor.core.extrinsics.serving import serve_extrinsic
from deployment_layer.circuit_store import circuit_store
from execution_layer.dsperse_manager import DSperseManager
from execution_layer.base_input import BaseInput
from execution_layer.verified_model_session import VerifiedModelSession
from fastapi.responses import JSONResponse
from protocol import (
    Competition,
    DSliceProofGenerationDataModel,
    ProofOfWeightsDataModel,
    QueryForCapacities,
    QueryZkProof,
)
from rich.console import Console
from rich.table import Table

from _validator.models.request_type import RequestType
from constants import (
    ONE_HOUR,
    SINGLE_PROOF_OF_WEIGHTS_MODEL_ID,
)
from utils import AutoUpdate, clean_temp_files, wandb_logger
from utils.rate_limiter import with_rate_limit
from .circuit_manager import CircuitManager

COMPETITION_DIR = os.path.join(
    os.path.dirname(__file__), "..", "..", "competition_circuit"
)


class MinerSession:

    def __init__(self):
        self.configure()
        self.check_register(should_exit=True)
        self.auto_update = AutoUpdate()
        self.dsperse_manager = DSperseManager()
        self.log_batch = []
        self.shuffled_uids = None
        self.last_shuffle_epoch = -1
        if cli_parser.config.disable_blacklist:
            bt.logging.warning(
                "Blacklist disabled, allowing all requests. Consider enabling to filter requests."
            )
        websocket.setdefaulttimeout(30)

    def start_server(self) -> bool:
        if self.server.started:
            bt.logging.debug("Server already started, skipping start_server call")
            return True

        bt.logging.info(
            "Starting server. Custom arguments include the following.\n"
            "Note that any null values will fallback to defaults, "
            f"which are usually sufficient. {cli_parser.config.axon}"
        )

        self.server.register_route(
            path=f"/{QueryZkProof.name}", endpoint=self.query_zk_proof
        )
        self.server.register_route(
            path=f"/{ProofOfWeightsDataModel.name}", endpoint=self.handle_pow_request
        )
        self.server.register_route(
            path=f"/{Competition.name}", endpoint=self.handle_competition_request
        )
        self.server.register_route(
            path=f"/{QueryForCapacities.name}", endpoint=self.handle_capacity_request
        )
        self.server.register_route(
            path=f"/{DSliceProofGenerationDataModel.name}",
            endpoint=self.handle_dslice_request,
        )
        self.server.start()

        existing_miner = self.metagraph.axons[self.subnet_uid]

        if (
            existing_miner
            and existing_miner.port == self.server.external_port
            and existing_miner.ip == self.server.external_ip
        ):
            bt.logging.debug(
                f"Miner already serving on ip {self.server.external_ip} and port {self.server.external_port}"
            )
            return True
        bt.logging.info(
            f"Serving on network: {self.subtensor.chain_endpoint} with netuid: {cli_parser.config.netuid}"
        )

        # Subscribe to chain
        serve_success: bool = serve_extrinsic(
            subtensor=self.subtensor,
            wallet=self.wallet,
            ip=self.server.external_ip,
            port=self.server.external_port,
            protocol=4,
            netuid=cli_parser.config.netuid,
        )
        bt.logging.info(
            f"Serving on network: {self.subtensor.chain_endpoint} with netuid: {cli_parser.config.netuid}"
        )
        return serve_success

    def run(self):
        """
        Keep the miner alive.
        This loop maintains the miner's operations until intentionally stopped.
        """
        bt.logging.info("Starting miner...")
        self.start_server()

        step = 0

        while True:
            step += 1
            try:

                if step % 100 == 0:
                    if not cli_parser.config.no_auto_update:
                        self.auto_update.try_update()
                    else:
                        bt.logging.debug(
                            "Automatic updates are disabled, skipping version check"
                        )

                if step % 20 == 0:
                    if len(self.log_batch) > 0:
                        bt.logging.debug(
                            f"Logging batch to WandB of size {len(self.log_batch)}"
                        )
                        for log in self.log_batch:
                            wandb_logger.safe_log(log)
                        self.log_batch = []
                    else:
                        bt.logging.debug("No logs to log to WandB")

                if step % 600 == 0:
                    self.check_register()

                if step % 24 == 0 and self.subnet_uid is not None:
                    table = Table(title=f"Miner Status (UID: {self.subnet_uid})")
                    table.add_column("Block", justify="center", style="cyan")
                    table.add_column("Stake", justify="center", style="cyan")
                    table.add_column("Trust", justify="center", style="cyan")
                    table.add_column("Consensus", justify="center", style="cyan")
                    table.add_column("Incentive", justify="center", style="cyan")
                    table.add_column("Emission", justify="center", style="cyan")
                    table.add_row(
                        str(self.metagraph.block.item()),
                        str(self.metagraph.S[self.subnet_uid]),
                        str(self.metagraph.TS[self.subnet_uid]),
                        str(self.metagraph.C[self.subnet_uid]),
                        str(self.metagraph.I[self.subnet_uid]),
                        str(self.metagraph.E[self.subnet_uid]),
                    )
                    console = Console()
                    console.print(table)
                self.sync_metagraph()

                time.sleep(1)

            except KeyboardInterrupt:
                bt.logging.success("Miner killed via keyboard interrupt.")
                if self.server.started:
                    self.server.stop()
                clean_temp_files()
                break
            except Exception as e:
                bt.logging.error(f"Error in main loop: {e}")
                traceback.print_exc()
                continue

    def check_register(self, should_exit=False):
        if self.wallet.hotkey.ss58_address not in self.metagraph.hotkeys:
            bt.logging.error(
                f"\nYour miner: {self.wallet} is not registered to the network: {self.subtensor} \n"
                "Run btcli register and try again."
            )
            if should_exit:
                exit()
            self.subnet_uid = None
        else:
            subnet_uid = self.metagraph.hotkeys.index(self.wallet.hotkey.ss58_address)
            self.subnet_uid = subnet_uid

    def configure(self):
        self.wallet = bt.Wallet(config=cli_parser.config)
        self.subtensor = bt.Subtensor(config=cli_parser.config)
        self.metagraph: bt.Metagraph = self.subtensor.metagraph(
            cli_parser.config.netuid
        )
        self.server = MinerServer(
            wallet=self.wallet, config=cli_parser.config, metagraph=self.metagraph
        )
        wandb_logger.safe_init("Miner", self.wallet, self.metagraph, cli_parser.config)

        if cli_parser.config.storage:
            storage_config = {
                "provider": cli_parser.config.storage.provider,
                "bucket": cli_parser.config.storage.bucket,
                "account_id": cli_parser.config.storage.account_id,
                "access_key": cli_parser.config.storage.access_key,
                "secret_key": cli_parser.config.storage.secret_key,
                "region": cli_parser.config.storage.region,
            }
        else:
            bt.logging.warning(
                "No storage config provided, circuit manager will not be initialized."
            )
            storage_config = None

        try:
            current_commitment = self.subtensor.get_commitment(
                cli_parser.config.netuid,
                self.metagraph.hotkeys.index(self.wallet.hotkey.ss58_address),
            )

            self.circuit_manager = CircuitManager(
                wallet=self.wallet,
                netuid=cli_parser.config.netuid,
                circuit_dir=COMPETITION_DIR,
                storage_config=storage_config,
                existing_vk_hash=current_commitment,
            )
        except Exception as e:
            traceback.print_exc()
            bt.logging.error(f"Error initializing circuit manager: {e}")
            self.circuit_manager = None

    def _load_circuit(self, model_id: str):
        try:
            circuit = circuit_store.ensure_circuit(model_id)
        except (ValueError, KeyError) as e:
            bt.logging.warning(f"Invalid circuit ID {model_id}: {e}")
            return None, JSONResponse(
                content=f"Invalid circuit ID: {model_id}", status_code=422
            )
        except Exception as e:
            bt.logging.error(f"Server error loading circuit {model_id}: {e}")
            traceback.print_exc()
            return None, JSONResponse(content="Internal server error", status_code=500)
        return circuit, None

    @with_rate_limit(period=ONE_HOUR)
    def sync_metagraph(self):
        try:
            self.metagraph.sync(subtensor=self.subtensor)
            return True
        except Exception as e:
            bt.logging.warning(f"Failed to sync metagraph: {e}")
            return False

    def handle_capacity_request(self) -> JSONResponse:
        """
        Handle capacity request from validators.
        """
        return JSONResponse(content=QueryForCapacities.from_config())

    def handle_competition_request(self, data: Competition) -> JSONResponse:
        """
        Handle competition circuit requests from validators.

        This endpoint provides signed URLs for validators to download circuit files.
        The process ensures:
        1. Files are uploaded to R2/S3
        2. VK hash matches chain commitment
        3. URLs are signed and time-limited
        4. All operations are thread-safe
        """
        bt.logging.info(
            f"Handling competition request for id={data.id} hash={data.hash}"
        )
        content = {
            "id": data.id,
            "hash": data.hash,
            "file_name": data.file_name,
        }
        try:
            if not self.circuit_manager:
                bt.logging.critical(
                    "Circuit manager not initialized, unable to respond to validator."
                )
                return JSONResponse(
                    content={"error": "Circuit manager not initialized", **content},
                    status_code=503,
                )

            bt.logging.info("Getting current commitment from circuit manager")
            commitment = self.circuit_manager.get_current_commitment()
            if not commitment:
                bt.logging.critical(
                    "No valid circuit commitment available. Unable to respond to validator."
                )
                return JSONResponse(
                    content={
                        "error": "No valid circuit commitment available",
                        **content,
                    },
                    status_code=503,
                )

            bt.logging.info("Getting chain commitment from subtensor")
            chain_commitment = self.subtensor.get_commitment(
                cli_parser.config.netuid,
                self.metagraph.hotkeys.index(self.wallet.hotkey.ss58_address),
            )
            if commitment.vk_hash != chain_commitment:
                bt.logging.critical(
                    f"Hash mismatch - local: {commitment.vk_hash[:8]} "
                    f"chain: {chain_commitment[:8]}"
                )
                return JSONResponse(
                    content={
                        "error": "Hash mismatch between local and chain commitment",
                        **content,
                    },
                    status_code=503,
                )

            bt.logging.info("Generating signed URLs for required files")
            required_files = ["settings.json", "model.compiled"]
            object_keys = {}
            for file_name in required_files:
                object_keys[file_name] = f"{commitment.vk_hash}/{file_name}"
            signed_urls = self.circuit_manager._get_signed_urls(object_keys)
            if not signed_urls:
                bt.logging.error("Failed to get signed URLs")
                return JSONResponse(
                    content={"error": "Failed to get signed URLs", **content},
                    status_code=503,
                )

            bt.logging.info("Preparing commitment data response")
            commitment_data = commitment.model_dump()
            commitment_data["signed_urls"] = signed_urls

            bt.logging.info("Successfully prepared competition response")
            return JSONResponse(
                content={
                    "commitment": json.dumps(commitment_data),
                    "error": None,
                    **content,
                }
            )

        except Exception as e:
            bt.logging.error(f"Error handling competition request: {str(e)}")
            traceback.print_exc()
            return JSONResponse(
                content={"error": "An internal error occurred.", **content},
                status_code=500,
            )

    def handle_dslice_request(
        self, data: DSliceProofGenerationDataModel
    ) -> JSONResponse:
        """
        Handle DSlice proof generation requests from validators.
        """
        try:
            bt.logging.info(
                f"Handling DSlice proof generation request for slice_num={data.slice_num} run_uid={data.run_uid}"
            )

            result = self.dsperse_manager.prove_slice(
                circuit_id=data.circuit,
                slice_num=data.slice_num,
                inputs=data.inputs,
                outputs=data.outputs,
                proof_system=data.proof_system,
            )

            return JSONResponse(content=result, status_code=200)
        except Exception as e:
            bt.logging.error(f"Error handling DSlice request: {str(e)}")
            traceback.print_exc()
            return JSONResponse(
                content={"error": "An internal error occurred."}, status_code=500
            )

    def _generate_proof(
        self, model_id: str, inputs: dict
    ) -> tuple[JSONResponse | None, dict | None]:
        circuit, error = self._load_circuit(model_id)
        if error:
            return error, None

        try:
            bt.logging.info(f"Running proof generation for {circuit}")
            model_session = VerifiedModelSession(
                BaseInput(RequestType.RWR, inputs), circuit
            )
            proof, public, proof_time = model_session.gen_proof()
            if isinstance(proof, bytes):
                proof = proof.hex()
            model_session.end()
            bt.logging.info(f"Proof completed for {circuit}")
        except Exception as e:
            bt.logging.error(f"An error occurred while generating proof\n{e}")
            traceback.print_exc()
            return JSONResponse(content="An error occurred", status_code=500), None

        return None, {
            "proof": proof,
            "public_signals": public,
            "proof_time": proof_time,
            "circuit_timeout": circuit.timeout,
        }

    def _log_proof_timing(
        self, model_id: str, time_in: float, proof_time: float, circuit_timeout: float
    ):
        delta_t = time.time() - time_in
        bt.logging.info(
            f"Total response time {delta_t}s. Proof time: {proof_time}s. "
            f"Overhead time: {delta_t - proof_time}s."
        )
        self.log_batch.append(
            {
                model_id: {
                    "proof_time": proof_time,
                    "overhead_time": delta_t - proof_time,
                    "total_response_time": delta_t,
                }
            }
        )
        if delta_t > circuit_timeout:
            bt.logging.error(
                "Response time is greater than circuit timeout. "
                "This indicates your hardware is not processing the requests in time."
            )

    def query_zk_proof(self, data: QueryZkProof) -> JSONResponse:
        time_in = time.time()
        if not data.query_input:
            return JSONResponse(content="Empty query input", status_code=422)

        model_id = str(data.model_id or SINGLE_PROOF_OF_WEIGHTS_MODEL_ID)
        error, result = self._generate_proof(model_id, data.query_input)
        if error:
            return error

        self._log_proof_timing(
            model_id, time_in, result["proof_time"], result["circuit_timeout"]
        )
        return JSONResponse(
            content={
                "proof": result["proof"],
                "public_signals": result["public_signals"],
            }
        )

    def handle_pow_request(self, data: ProofOfWeightsDataModel) -> JSONResponse:
        time_in = time.time()
        if not data.inputs:
            return JSONResponse(
                content="Empty input for proof of weights", status_code=422
            )

        model_id = str(data.verification_key_hash)
        error, result = self._generate_proof(model_id, data.inputs)
        if error:
            return error

        self._log_proof_timing(
            model_id, time_in, result["proof_time"], result["circuit_timeout"]
        )
        return JSONResponse(
            content={
                "inputs": data.inputs,
                "proof": result["proof"],
                "public_signals": result["public_signals"],
            }
        )
