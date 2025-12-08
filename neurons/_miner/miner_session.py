# from __future__ import annotations
import json
import os
import time
import traceback
from typing import Tuple, Union

import bittensor as bt
import websocket
from rich.console import Console
from rich.table import Table

import cli_parser
from _validator.models.request_type import RequestType
from constants import (
    SINGLE_PROOF_OF_WEIGHTS_MODEL_ID,
    STEAK,
    VALIDATOR_STAKE_THRESHOLD,
    ONE_HOUR,
    CIRCUIT_TIMEOUT_SECONDS,
    NUM_MINER_GROUPS,
    MINER_RESET_WINDOW_BLOCKS,
    ONE_MINUTE,
)
from deployment_layer.circuit_store import circuit_store
from execution_layer.generic_input import GenericInput
from execution_layer.verified_model_session import VerifiedModelSession
from protocol import (
    QueryZkProof,
    ProofOfWeightsSynapse,
    Competition,
    QueryForCapacities,
)
from utils import AutoUpdate, clean_temp_files, wandb_logger
from utils.rate_limiter import with_rate_limit
from utils.epoch import get_current_epoch_info, get_epoch_start_block
from .circuit_manager import CircuitManager
from utils.shuffle import get_shuffled_uids

COMPETITION_DIR = os.path.join(
    os.path.dirname(__file__), "..", "..", "competition_circuit"
)


class MinerSession:

    axon: Union[bt.axon, None] = None

    def __init__(self):
        self.configure()
        self.check_register(should_exit=True)
        self.auto_update = AutoUpdate()
        self.log_batch = []
        self.shuffled_uids = None
        self.last_shuffle_epoch = -1
        if cli_parser.config.disable_blacklist:
            bt.logging.warning(
                "Blacklist disabled, allowing all requests. Consider enabling to filter requests."
            )
        websocket.setdefaulttimeout(30)

    def start_axon(self):
        bt.logging.info(
            "Starting axon. Custom arguments include the following.\n"
            "Note that any null values will fallback to defaults, "
            f"which are usually sufficient. {cli_parser.config.axon}"
        )

        axon = bt.axon(wallet=self.wallet, config=cli_parser.config)
        bt.logging.info(f"Axon created: {axon.info()}")

        bt.logging.info("Attaching forward functions to axon...")
        axon.attach(forward_fn=self.queryZkProof, blacklist_fn=self.proof_blacklist)
        axon.attach(
            forward_fn=self.handle_pow_request,
            blacklist_fn=self.pow_blacklist,
        )
        axon.attach(
            forward_fn=self.handleCompetitionRequest,
            blacklist_fn=self.competition_blacklist,
        )
        axon.attach(
            forward_fn=self.handleCapacityRequest,
            blacklist_fn=self.capacity_blacklist,
        )

        bt.logging.info("Attached forward functions to axon")

        bt.logging.info(f"Starting axon server: {axon.info()}")
        axon.start()
        bt.logging.info(f"Started axon server: {axon.info()}")

        existing_axon = self.metagraph.axons[self.subnet_uid]

        if (
            existing_axon
            and existing_axon.port == axon.external_port
            and existing_axon.ip == axon.external_ip
        ):
            bt.logging.debug(
                f"Axon already serving on ip {axon.external_ip} and port {axon.external_port}"
            )
            return
        bt.logging.info(
            f"Serving axon on network: {self.subtensor.chain_endpoint} with netuid: {cli_parser.config.netuid}"
        )

        axon.serve(netuid=cli_parser.config.netuid, subtensor=self.subtensor)
        bt.logging.info(
            f"Served axon on network: {self.subtensor.chain_endpoint} with netuid: {cli_parser.config.netuid}"
        )

        self.axon = axon

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
        self.start_axon()

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

    def proof_blacklist(self, synapse: QueryZkProof) -> Tuple[bool, str]:
        """
        Blacklist method for the proof generation endpoint
        """
        return self._blacklist(synapse)

    def pow_blacklist(self, synapse: ProofOfWeightsSynapse) -> Tuple[bool, str]:
        """
        Blacklist method for the proof generation endpoint
        """
        return self._blacklist(synapse)

    def competition_blacklist(self, synapse: Competition) -> Tuple[bool, str]:
        """
        Blacklist method for the competition endpoint
        """
        return self._blacklist(synapse)

    def capacity_blacklist(self, synapse: QueryForCapacities) -> Tuple[bool, str]:
        """
        Blacklist method for the capacity request endpoint
        """
        return self._blacklist(synapse)

    def _blacklist(
        self,
        synapse: Union[
            QueryZkProof,
            ProofOfWeightsSynapse,
            QueryForCapacities,
            Competition,
        ],
    ) -> Tuple[bool, str]:
        """
        Filters requests if any of the following conditions are met:
        - Requesting hotkey is not registered
        - Requesting UID's stake is below 1k
        - Requesting UID does not have a validator permit

        Does not filter if the --disable-blacklist flag has been set.

        synapse: The request synapse object
        returns: (is_blacklisted, reason)
        """
        try:
            if cli_parser.config.disable_blacklist:
                bt.logging.trace("Blacklist disabled, allowing request.")
                return False, "Allowed"

            if synapse.dendrite.hotkey not in self.metagraph.hotkeys:  # type: ignore
                return True, "Hotkey is not registered"

            requesting_uid = self.metagraph.hotkeys.index(synapse.dendrite.hotkey)  # type: ignore
            stake = self.metagraph.S[requesting_uid].item()

            try:
                bt.logging.info(
                    f"Request by: {synapse.dendrite.hotkey} | UID: {requesting_uid} "  # type: ignore
                    f"| Stake: {stake} {STEAK}"
                )
            except UnicodeEncodeError:
                bt.logging.info(
                    f"Request by: {synapse.dendrite.hotkey} | UID: {requesting_uid} | Stake: {stake}"  # type: ignore
                )

            if stake < VALIDATOR_STAKE_THRESHOLD:
                return True, "Stake below minimum"

            validator_permit = self.metagraph.validator_permit[requesting_uid].item()
            if not validator_permit:
                return True, "Requesting UID has no validator permit"

            bt.logging.trace(f"Allowing request from UID: {requesting_uid}")
            return False, "Allowed"

        except Exception as e:
            bt.logging.error(f"Error during blacklist {e}")
            return True, "An error occurred while filtering the request"

    def handleCapacityRequest(self, synapse: QueryForCapacities) -> QueryForCapacities:
        """
        Handle capacity request from validators.
        """
        synapse.capacities = QueryForCapacities.from_config()
        return synapse

    def handleCompetitionRequest(self, synapse: Competition) -> Competition:
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
            f"Handling competition request for id={synapse.id} hash={synapse.hash}"
        )
        try:
            if not self.circuit_manager:
                bt.logging.critical(
                    "Circuit manager not initialized, unable to respond to validator."
                )
                return Competition(
                    id=synapse.id,
                    hash=synapse.hash,
                    file_name=synapse.file_name,
                    error="Circuit manager not initialized",
                )

            bt.logging.info("Getting current commitment from circuit manager")
            commitment = self.circuit_manager.get_current_commitment()
            if not commitment:
                bt.logging.critical(
                    "No valid circuit commitment available. Unable to respond to validator."
                )
                return Competition(
                    id=synapse.id,
                    hash=synapse.hash,
                    file_name=synapse.file_name,
                    error="No valid circuit commitment available",
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
                return Competition(
                    id=synapse.id,
                    hash=synapse.hash,
                    file_name=synapse.file_name,
                    error="Hash mismatch between local and chain commitment",
                )

            bt.logging.info("Generating signed URLs for required files")
            required_files = ["settings.json", "model.compiled"]
            object_keys = {}
            for file_name in required_files:
                object_keys[file_name] = f"{commitment.vk_hash}/{file_name}"
            signed_urls = self.circuit_manager._get_signed_urls(object_keys)
            if not signed_urls:
                bt.logging.error("Failed to get signed URLs")
                return Competition(
                    id=synapse.id,
                    hash=synapse.hash,
                    file_name=synapse.file_name,
                    error="Failed to get signed URLs",
                )

            bt.logging.info("Preparing commitment data response")
            commitment_data = commitment.model_dump()
            commitment_data["signed_urls"] = signed_urls

            response = Competition(
                id=synapse.id,
                hash=synapse.hash,
                file_name=synapse.file_name,
                commitment=json.dumps(commitment_data),
            )
            bt.logging.info("Successfully prepared competition response")
            return response

        except Exception as e:
            bt.logging.error(f"Error handling competition request: {str(e)}")
            traceback.print_exc()
            return Competition(
                id=synapse.id,
                hash=synapse.hash,
                file_name=synapse.file_name,
                error=str(e),
            )

    def queryZkProof(self, synapse: QueryZkProof) -> QueryZkProof:
        """
        This function run proof generation of the model (with its output as well)
        """
        if cli_parser.config.competition_only:
            bt.logging.info("Competition only mode enabled. Skipping proof generation.")
            synapse.query_output = "Competition only mode enabled"
            return synapse

        time_in = time.time()
        bt.logging.debug("Received request from validator")
        bt.logging.debug(f"Input data: {synapse.query_input} \n")

        if not synapse.query_input or not synapse.query_input.get(
            "public_inputs", None
        ):
            bt.logging.error("Received empty query input")
            synapse.query_output = "Empty query input"
            return synapse

        model_id = synapse.query_input.get("model_id", SINGLE_PROOF_OF_WEIGHTS_MODEL_ID)
        public_inputs = synapse.query_input["public_inputs"]

        circuit_timeout = CIRCUIT_TIMEOUT_SECONDS
        try:
            circuit = circuit_store.get_circuit(str(model_id))
            if not circuit:
                raise ValueError(
                    f"Circuit {model_id} not found. This indicates a missing deployment layer folder or invalid request"
                )
            circuit_timeout = circuit.timeout
            bt.logging.info(f"Running proof generation for {circuit}")
            model_session = VerifiedModelSession(
                GenericInput(RequestType.RWR, public_inputs), circuit
            )
            bt.logging.debug("Model session created successfully")
            proof, public, proof_time = model_session.gen_proof()
            if isinstance(proof, bytes):
                proof = proof.hex()

            synapse.query_output = json.dumps(
                {
                    "proof": proof,
                    "public_signals": public,
                }
            )
            bt.logging.trace(f"Proof: {synapse.query_output}, Time: {proof_time}")
            model_session.end()
            try:
                bt.logging.info(f"✅ Proof completed for {circuit}\n")
            except UnicodeEncodeError:
                bt.logging.info(f"Proof completed for {circuit}\n")
        except Exception as e:
            synapse.query_output = "An error occurred"
            bt.logging.error(f"An error occurred while generating proven output\n{e}")
            traceback.print_exc()
            proof_time = time.time() - time_in

        time_out = time.time()
        delta_t = time_out - time_in
        bt.logging.info(
            f"Total response time {delta_t}s. Proof time: {proof_time}s. "
            f"Overhead time: {delta_t - proof_time}s."
        )
        self.log_batch.append(
            {
                str(model_id): {
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
        return synapse

    def handle_pow_request(
        self, synapse: ProofOfWeightsSynapse
    ) -> ProofOfWeightsSynapse:
        """
        Handles a proof of weights request
        """
        if cli_parser.config.competition_only:
            bt.logging.info("Competition only mode enabled. Skipping proof generation.")
            synapse.query_output = "Competition only mode enabled"
            return synapse

        time_in = time.time()
        bt.logging.debug("Received proof of weights request from validator")
        bt.logging.debug(f"Input data: {synapse.inputs} \n")

        if not synapse.inputs:
            bt.logging.error("Received empty input for proof of weights")
            return synapse

        circuit_timeout = CIRCUIT_TIMEOUT_SECONDS
        try:
            circuit = circuit_store.get_circuit(str(synapse.verification_key_hash))
            if not circuit:
                raise ValueError(
                    f"Circuit {synapse.verification_key_hash} not found. "
                    "This indicates a missing deployment layer folder or invalid request"
                )
            circuit_timeout = circuit.timeout
            bt.logging.info(f"Running proof generation for {circuit}")
            model_session = VerifiedModelSession(
                GenericInput(RequestType.RWR, synapse.inputs), circuit
            )

            bt.logging.debug("Model session created successfully")
            proof, public, proof_time = model_session.gen_proof()
            model_session.end()

            synapse.proof = proof.hex() if isinstance(proof, bytes) else proof
            synapse.public_signals = public
            bt.logging.info(f"✅ Proof of weights completed for {circuit}\n")
        except Exception as e:
            bt.logging.error(
                f"An error occurred while generating proof of weights\n{e}"
            )
            traceback.print_exc()
            proof_time = time.time() - time_in

        time_out = time.time()
        delta_t = time_out - time_in
        bt.logging.info(
            f"Total response time {delta_t}s. Proof time: {proof_time}s. "
            f"Overhead time: {delta_t - proof_time}s."
        )
        self.log_batch.append(
            {
                str(synapse.verification_key_hash): {
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
        return synapse
