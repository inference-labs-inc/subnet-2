# from __future__ import annotations
import json
import os
import time
import traceback

import bittensor as bt
import websocket
from bittensor.core.extrinsics.serving import serve_extrinsic
from fastapi.responses import JSONResponse
from rich.console import Console
from rich.table import Table

import cli_parser
from _miner.server import MinerServer
from _validator.models.request_type import RequestType
from constants import (
    CIRCUIT_TIMEOUT_SECONDS,
    MINER_RESET_WINDOW_BLOCKS,
    NUM_MINER_GROUPS,
    ONE_HOUR,
    ONE_MINUTE,
    SINGLE_PROOF_OF_WEIGHTS_MODEL_ID,
)
from deployment_layer.circuit_store import circuit_store
from execution_layer.dsperse_manager import DSperseManager
from execution_layer.generic_input import GenericInput
from execution_layer.verified_model_session import VerifiedModelSession
from protocol import (
    Competition,
    DSliceProofGenerationDataModel,
    ProofOfWeightsDataModel,
    QueryForCapacities,
    QueryZkProof,
)
from utils import AutoUpdate, clean_temp_files, wandb_logger
from utils.epoch import get_current_epoch_info, get_epoch_start_block
from utils.rate_limiter import with_rate_limit
from utils.shuffle import get_shuffled_uids
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
            path=f"/{QueryZkProof.name}", endpoint=self.queryZkProof
        )
        self.server.register_route(
            path=f"/{ProofOfWeightsDataModel.name}", endpoint=self.handle_pow_request
        )
        self.server.register_route(
            path=f"/{Competition.name}", endpoint=self.handleCompetitionRequest
        )
        self.server.register_route(
            path=f"/{QueryForCapacities.name}", endpoint=self.handleCapacityRequest
        )
        self.server.register_route(
            path=f"/{DSliceProofGenerationDataModel.name}",
            endpoint=self.handleDSliceRequest,
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

    def perform_reset(self):
        """
        Coordinated reset performed by all miners in
        the same group at synchronized block intervals.
        """
        bt.logging.info("Performing coordinated reset")
        try:
            commitment_info = [{"ResetBondsFlag": b""}]
            call = self.subtensor.substrate.compose_call(
                call_module="Commitments",
                call_function="set_commitment",
                call_params={
                    "netuid": cli_parser.config.netuid,
                    "info": {"fields": [commitment_info]},
                },
            )
            success, message = self.subtensor.sign_and_send_extrinsic(
                call=call,
                wallet=self.wallet,
                sign_with="hotkey",
                period=MINER_RESET_WINDOW_BLOCKS,
            )
            if not success:
                bt.logging.error(f"Failed to perform reset: {message}")
            else:
                bt.logging.success("Successfully performed reset")
        except Exception as e:
            bt.logging.error(f"Error performing reset: {e}")

    @with_rate_limit(period=ONE_MINUTE)
    def log_reset_check(self, current_block: int, current_epoch: int, miner_group: int):
        """Logs information about the next scheduled reset for the miner's group."""
        current_group_in_rotation = current_epoch % NUM_MINER_GROUPS
        epochs_until_next_turn = (
            miner_group - current_group_in_rotation + NUM_MINER_GROUPS
        ) % NUM_MINER_GROUPS

        if epochs_until_next_turn == 0:
            # This is our group's epoch, so the next one is a full cycle away.
            next_reset_epoch = current_epoch + NUM_MINER_GROUPS
        else:
            next_reset_epoch = current_epoch + epochs_until_next_turn

        next_reset_start_block = get_epoch_start_block(
            next_reset_epoch, cli_parser.config.netuid
        )
        blocks_until_next_reset = next_reset_start_block - current_block

        bt.logging.info(
            f"Group {miner_group} | "
            f"Current Block: {current_block} | "
            f"Next Reset Epoch: {next_reset_epoch} (starts at block ~{next_reset_start_block}) | "
            f"Blocks Until Reset: ~{blocks_until_next_reset}"
        )

    def perform_reset_check(self):
        if self.subnet_uid is None:
            return

        current_block = self.subtensor.get_current_block()
        (
            current_epoch,
            blocks_until_next_epoch,
            epoch_start_block,
        ) = get_current_epoch_info(current_block, cli_parser.config.netuid)

        (
            self.shuffled_uids,
            self.last_shuffle_epoch,
            shuffle_block,
            shuffle_hash,
        ) = get_shuffled_uids(
            current_epoch,
            self.last_shuffle_epoch,
            self.metagraph,
            self.subtensor,
            self.shuffled_uids,
        )

        bt.logging.info(f"Shuffle block: {shuffle_block}, shuffle hash: {shuffle_hash}")

        try:
            uid_index = self.shuffled_uids.index(self.subnet_uid)
            miner_group = uid_index % NUM_MINER_GROUPS
        except ValueError:
            bt.logging.error(
                f"Miner UID {self.subnet_uid} not found in shuffled UIDs. Skipping reset check."
            )
            return

        self.log_reset_check(current_block, current_epoch, miner_group)

        if current_epoch % NUM_MINER_GROUPS == miner_group:
            if blocks_until_next_epoch <= MINER_RESET_WINDOW_BLOCKS:
                last_bonds_submission = 0
                try:
                    last_bonds_submission = self.subtensor.substrate.query(
                        "Commitments",
                        "LastBondsReset",
                        params=[
                            cli_parser.config.netuid,
                            self.wallet.hotkey.ss58_address,
                        ],
                    )
                except Exception as e:
                    bt.logging.error(f"Error querying last bonds submission: {e}")

                if (
                    not last_bonds_submission
                    or last_bonds_submission < epoch_start_block
                ):
                    bt.logging.info(
                        f"Current block: {current_block}, epoch: {current_epoch}, "
                        f"group {miner_group} reset trigger "
                        f"(blocks until next epoch: {blocks_until_next_epoch})"
                    )
                    self.perform_reset()

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
                if step % 10 == 0:
                    self.perform_reset_check()

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
                    table.add_column("Rank", justify="center", style="cyan")
                    table.add_column("Trust", justify="center", style="cyan")
                    table.add_column("Consensus", justify="center", style="cyan")
                    table.add_column("Incentive", justify="center", style="cyan")
                    table.add_column("Emission", justify="center", style="cyan")
                    table.add_row(
                        str(self.metagraph.block.item()),
                        str(self.metagraph.S[self.subnet_uid]),
                        str(self.metagraph.R[self.subnet_uid]),
                        str(self.metagraph.T[self.subnet_uid]),
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
            except Exception:
                bt.logging.error(traceback.format_exc())
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
        self.wallet = bt.wallet(config=cli_parser.config)
        self.subtensor = bt.subtensor(config=cli_parser.config)
        self.metagraph = self.subtensor.metagraph(cli_parser.config.netuid)
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

    @with_rate_limit(period=ONE_HOUR)
    def sync_metagraph(self):
        try:
            self.metagraph.sync(subtensor=self.subtensor)
            return True
        except Exception as e:
            bt.logging.warning(f"Failed to sync metagraph: {e}")
            return False

    def handleCapacityRequest(self) -> JSONResponse:
        """
        Handle capacity request from validators.
        """
        return JSONResponse(content=QueryForCapacities.from_config())

    def handleCompetitionRequest(self, data: Competition) -> JSONResponse:
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

    def handleDSliceRequest(self, data: DSliceProofGenerationDataModel) -> JSONResponse:
        """
        Handle DSlice proof generation requests from validators.
        """
        bt.logging.info(
            f"Handling DSlice proof generation request for slice_num={data.slice_num} run_uid={data.run_uid}"
        )

        result = self.dsperse_manager.prove_slice(
            circuit_id=data.circuit,
            slice_num=data.slice_num,
            inputs=data.inputs,
            outputs=data.outputs,
        )

        # Implementation for handling DSlice slice requests goes here
        return JSONResponse(content=result, status_code=200)

    def queryZkProof(self, data: QueryZkProof) -> JSONResponse:
        """
        This function run proof generation of the model (with its output as well)
        """
        if cli_parser.config.competition_only:
            bt.logging.info("Competition only mode enabled. Skipping proof generation.")
            return JSONResponse(
                content="Competition only mode enabled", status_code=422
            )

        time_in = time.time()
        bt.logging.debug("Received request from validator")
        bt.logging.debug(f"Input data: {data.query_input} \n")

        if not data.query_input:
            bt.logging.error("Received empty query input")
            return JSONResponse(content="Empty query input", status_code=422)

        model_id = str(data.model_id or SINGLE_PROOF_OF_WEIGHTS_MODEL_ID)
        circuit_timeout = CIRCUIT_TIMEOUT_SECONDS
        try:
            circuit = circuit_store.get_circuit(model_id)
            if not circuit:
                return JSONResponse(
                    content=f"'{model_id}' Circuit not found", status_code=422
                )

            circuit_timeout = circuit.timeout
            bt.logging.info(f"Running proof generation for {circuit}")
            model_session = VerifiedModelSession(
                GenericInput(RequestType.RWR, data.query_input), circuit
            )
            bt.logging.debug("Model session created successfully")
            proof, public, proof_time = model_session.gen_proof()
            if isinstance(proof, bytes):
                proof = proof.hex()

            output = {
                "proof": proof,
                "public_signals": public,
            }
            bt.logging.trace(f"Proof: {output}, Time: {proof_time}")
            model_session.end()
            try:
                bt.logging.info(f"✅ Proof completed for {circuit}\n")
            except UnicodeEncodeError:
                bt.logging.info(f"Proof completed for {circuit}\n")
        except Exception as e:
            bt.logging.error(f"An error occurred while generating proven output\n{e}")
            traceback.print_exc()
            return JSONResponse(content="An error occurred", status_code=500)

        time_out = time.time()
        delta_t = time_out - time_in
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
        return JSONResponse(content=output)

    def handle_pow_request(self, data: ProofOfWeightsDataModel) -> JSONResponse:
        """
        Handles a proof of weights request
        """
        if cli_parser.config.competition_only:
            bt.logging.info("Competition only mode enabled. Skipping proof generation.")
            return JSONResponse(
                content="Competition only mode enabled", status_code=422
            )

        time_in = time.time()
        bt.logging.debug("Received proof of weights request from validator")
        bt.logging.debug(f"Input data: {data.inputs} \n")

        if not data.inputs:
            bt.logging.error("Received empty input for proof of weights")
            return JSONResponse(
                content="Empty input for proof of weights", status_code=422
            )
        circuit_timeout = CIRCUIT_TIMEOUT_SECONDS
        response = {}
        try:
            circuit = circuit_store.get_circuit(str(data.verification_key_hash))
            if not circuit:
                return JSONResponse(content="Circuit not found", status_code=422)
            circuit_timeout = circuit.timeout
            bt.logging.info(f"Running proof generation for {circuit}")
            model_session = VerifiedModelSession(
                GenericInput(RequestType.RWR, data.inputs), circuit
            )

            bt.logging.debug("Model session created successfully")
            proof, public, proof_time = model_session.gen_proof()
            model_session.end()

            bt.logging.info(f"✅ Proof of weights completed for {circuit}\n")
            response["inputs"] = data.inputs
            response["proof"] = proof.hex() if isinstance(proof, bytes) else proof
            response["public_signals"] = public
        except Exception as e:
            bt.logging.error(
                f"An error occurred while generating proof of weights\n{e}"
            )
            traceback.print_exc()
            return JSONResponse(content="An error occurred", status_code=500)

        time_out = time.time()
        delta_t = time_out - time_in
        bt.logging.info(
            f"Total response time {delta_t}s. Proof time: {proof_time}s. "
            f"Overhead time: {delta_t - proof_time}s."
        )
        self.log_batch.append(
            {
                str(data.verification_key_hash): {
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
        return JSONResponse(content=response)
