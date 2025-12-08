import os
import shutil
from typing import Dict, Tuple, List, Optional
import bittensor as bt
import platform
import asyncio
import json
import traceback
import threading
from multiprocessing import Queue as MPQueue
from queue import Empty
import time

try:
    from async_substrate_interface.types import ScaleObj
except ImportError:
    ScaleObj = None
from substrateinterface.utils.ss58 import ss58_encode

from .models.neuron import NeuronState
from .services.circuit_validator import CircuitValidator
from .services.circuit_manager import CircuitManager
from bittensor.core.chain_data import decode_account_id
from .services.circuit_evaluator import CircuitEvaluator
from .services.sota_manager import SotaManager

from .utils.cleanup import cleanup_temp_dir
from constants import VALIDATOR_REQUEST_TIMEOUT_SECONDS
from _validator.models.request_type import ValidatorMessage
from utils.system import get_temp_folder


class CompetitionThread(threading.Thread):
    def __init__(
        self,
        competition: "Competition",
        config: bt.config,
    ):
        super().__init__()
        bt.logging.info("=== Competition Thread Constructor Start ===")
        self.competition = competition
        self._should_run = threading.Event()
        self._should_run.set()
        self.task_queue = MPQueue()
        self.daemon = True
        self.validator_to_competition_queue = None  # Messages FROM validator
        self.competition_to_validator_queue = None  # Messages TO validator
        self._loop = None
        self._loop_lock = threading.Lock()

        self.subtensor = bt.subtensor(config=config)

        bt.logging.info("Competition thread initialized with:")
        bt.logging.info(f"- Competition ID: {self.competition.competition_id}")
        bt.logging.info(f"- Competition Dir: {self.competition.competition_directory}")
        bt.logging.info(f"- Thread Daemon: {self.daemon}")
        bt.logging.info("=== Competition Thread Constructor End ===")

    def _get_loop(self):
        with self._loop_lock:
            if self._loop is None or self._loop.is_closed():
                self._loop = asyncio.new_event_loop()
            return self._loop

    def set_validator_message_queues(
        self,
        validator_to_competition_queue: MPQueue,
        competition_to_validator_queue: MPQueue,
    ):
        self.validator_to_competition_queue = validator_to_competition_queue
        self.competition_to_validator_queue = competition_to_validator_queue

    def wait_for_message(
        self,
        expected_message: ValidatorMessage,
        timeout: float = VALIDATOR_REQUEST_TIMEOUT_SECONDS,
    ) -> bool:
        start_time = time.time()
        while time.time() - start_time < timeout:
            try:
                message = self.validator_to_competition_queue.get(timeout=0.1)
                if message == expected_message:
                    return True
            except Empty:
                continue
        bt.logging.error(f"Timeout waiting for {expected_message} message")
        return False

    def _process_next_download(self):
        if not self.competition.current_download and self.competition.download_queue:
            uid, hotkey, hash = self.competition.download_queue.pop(0)
            self.competition.current_download = (uid, hotkey, hash)
            bt.logging.info(
                f"Processing download for circuit {hash[:8]} from {hotkey[:8]}"
            )

            try:
                circuit_dir = os.path.join(self.competition.temp_directory, hash)
                os.makedirs(circuit_dir, exist_ok=True)

                axon = self.competition.metagraph.axons[uid]

                loop = self._get_loop()
                try:
                    download_success = loop.run_until_complete(
                        self.competition.circuit_manager.download_files(
                            axon, hash, circuit_dir
                        )
                    )
                except Exception as e:
                    bt.logging.error(f"Error during download: {str(e)}")
                    bt.logging.error(f"Stack trace: {traceback.format_exc()}")
                    download_success = False

                if download_success:
                    bt.logging.info("Pausing request processing for evaluation...")
                    if self.competition_to_validator_queue:
                        self.competition_to_validator_queue.put(
                            ValidatorMessage.WINDDOWN
                        )
                        bt.logging.info("Waiting for validator winddown to complete...")

                        if not self.wait_for_message(
                            ValidatorMessage.WINDDOWN_COMPLETE
                        ):
                            bt.logging.error(
                                "Failed to wait for validator winddown, proceeding anyway"
                            )

                    bt.logging.info("Starting circuit evaluation...")
                    try:
                        uid, hotkey, hash = self.competition.current_download
                        self.competition._evaluate_circuit(
                            circuit_dir=circuit_dir,
                            circuit_uid=str(uid),
                            circuit_owner=hotkey,
                        )
                        self.competition.cleanup_circuit_dir(circuit_dir)
                    except Exception as e:
                        bt.logging.error(f"Error during circuit evaluation: {str(e)}")
                        bt.logging.error(f"Stack trace: {traceback.format_exc()}")
                    finally:
                        bt.logging.info("Resuming request processing...")
                        if self.competition_to_validator_queue:
                            self.competition_to_validator_queue.put(
                                ValidatorMessage.COMPETITION_COMPLETE
                            )
                else:
                    bt.logging.error(
                        f"Circuit download or evaluation failed for {hash[:8]} from {hotkey[:8]}"
                    )
            finally:
                self.competition.clear_current_download()

    def run(self):
        bt.logging.info("Competition thread starting...")

        while self._should_run.is_set():
            try:
                if (
                    not self.competition.download_queue
                    and not self.competition.current_download
                ):
                    time.sleep(1)
                    continue

                self._process_next_download()

            except Exception as e:
                bt.logging.error(f"Error in competition thread cycle: {str(e)}")
                bt.logging.error(f"Stack trace: {traceback.format_exc()}")
                time.sleep(5)

        bt.logging.info("Competition thread stopped")

    def stop(self):
        bt.logging.info("=== Competition Thread Stop Start ===")
        self._should_run.clear()
        with self._loop_lock:
            if self._loop and not self._loop.is_closed():
                self._loop.stop()
                self._loop.close()
        bt.logging.info("Competition thread stop signal sent")
        bt.logging.info("=== Competition Thread Stop End ===")


class Competition:
    def __init__(
        self,
        competition_id: int,
        metagraph: bt.metagraph,
        wallet: bt.wallet,
        config: bt.config,
    ):
        bt.logging.info("=== Competition Module Initialization Start ===")
        bt.logging.info(f"Initializing competition module with ID {competition_id}...")
        self.competition_id = competition_id
        self.metagraph = metagraph
        self.wallet = wallet
        self.dendrite = bt.dendrite(wallet=wallet)
        self.competition_directory = os.path.join(
            os.path.dirname(__file__), str(competition_id)
        )
        self.temp_directory = os.path.join(get_temp_folder(), str(competition_id))
        self.sota_directory = os.path.join(self.competition_directory, "sota")

        if not os.path.exists(self.temp_directory):
            os.makedirs(self.temp_directory)

        if not os.path.exists(self.sota_directory):
            os.makedirs(self.sota_directory)

        bt.logging.info("Loading config and initializing subtensor...")
        config_path = os.path.join(
            self.competition_directory, "competition_config.json"
        )
        with open(config_path) as f:
            self.config = json.load(f)
            self.accuracy_weight = (
                self.config.get("evaluation", {})
                .get("scoring_weights", {})
                .get("accuracy", 0.4)
            )
            bt.logging.debug(f"Loaded accuracy weight: {self.accuracy_weight}")

        self.sota_manager = SotaManager(self.sota_directory)
        self.circuit_manager = CircuitManager(
            self.temp_directory, self.competition_id, self.dendrite
        )
        self.circuit_validator = CircuitValidator()

        self.circuit_evaluator = CircuitEvaluator(
            self.config, self.competition_directory, self.sota_manager
        )

        self.miner_states: Dict[str, NeuronState] = (
            self.sota_manager.load_miner_states()
        )

        self.circuits_evaluated_count: int = 0
        self.is_active: bool = (
            self.config.get("start_timestamp")
            < time.time()
            < self.config.get("end_timestamp")
        )

        self.download_queue: List[Tuple[int, str, str]] = []
        self.current_download: Optional[Tuple[int, str, str]] = None
        self.download_complete = asyncio.Event()
        self.download_task: Optional[asyncio.Task] = None
        self.download_lock = asyncio.Lock()

        bt.logging.info("=== Starting Competition Thread ===")
        self.competition_thread = CompetitionThread(self, config)
        bt.logging.info("Competition thread instance created")
        self.competition_thread.start()
        bt.logging.info("=== Competition Thread Started ===")

    def fetch_commitments(self) -> List[Tuple[int, str, str]]:
        if platform.system() != "Darwin" and platform.machine() != "arm64":
            bt.logging.critical(
                "Competitions are only supported on macOS arm64 architecture."
            )
        if not self.is_active:
            bt.logging.info("Competition is not active, skipping commitment fetch")
            return []

        queryable_uids = self.metagraph.uids
        hotkey_to_uid = {self.metagraph.hotkeys[uid]: uid for uid in queryable_uids}
        self.miner_states = {
            k: v for k, v in self.miner_states.items() if k in self.metagraph.hotkeys
        }

        commitments = []

        try:
            commitment_map = self.competition_thread.subtensor.substrate.query_map(
                module="Commitments",
                storage_function="CommitmentOf",
                params=[self.metagraph.netuid],
            )
            if not commitment_map or not hasattr(commitment_map, "__iter__"):
                return []

            for result in commitment_map:
                if not result or len(result) != 2:
                    continue

                acc, info = result
                if not acc:
                    continue

                try:
                    if not isinstance(acc, (list, tuple)) or not acc:
                        continue
                    acc_bytes = bytes(
                        acc[0] if isinstance(acc[0], (list, tuple)) else acc
                    )
                    hotkey = decode_account_id(acc_bytes)
                    if ss58_encode(acc_bytes) != hotkey:
                        continue

                    if hotkey not in hotkey_to_uid:
                        continue

                    uid = hotkey_to_uid[hotkey]
                    hash = self._extract_hash(info)

                    if not hash:
                        continue

                    if self._is_new_valid_circuit(hotkey, hash):
                        commitments.append((uid, hotkey, hash))
                        bt.logging.success(
                            f"New circuit detected for {hotkey} with hash {hash}"
                        )

                except Exception as e:
                    bt.logging.debug(f"Failed to process commitment: {str(e)}")
                    continue

        except Exception as e:
            bt.logging.error(f"Error fetching commitments: {str(e)}")
            bt.logging.debug(traceback.format_exc())

        return commitments

    def _extract_hash(self, info) -> Optional[str]:
        """Extract hash from commitment info based on network type."""
        try:
            if not isinstance(info, (dict, ScaleObj)):
                return None

            info_val = info.value if isinstance(info, ScaleObj) else info
            fields = info_val.get("info", {}).get("fields", [])

            if not fields or not fields[0]:
                return None

            commitment = fields[0][0]
            if not commitment or not isinstance(commitment, dict):
                return None

            commitment_type = next(iter(commitment.keys()))
            bytes_tuple = commitment[commitment_type][0]

            if not isinstance(bytes_tuple, (list, tuple)):
                return None

            return bytes(bytes_tuple).decode()

        except Exception:
            return None

    def _is_new_valid_circuit(self, hotkey: str, hash: str) -> bool:
        """Check if circuit is new and valid."""
        if hotkey not in self.miner_states or self.miner_states[hotkey].hash != hash:
            if hash in {state.hash for state in self.miner_states.values()}:
                bt.logging.warning(
                    f"Circuit with hash {hash} already exists for another miner"
                )
                return False
            return True
        return False

    async def process_downloads(self) -> bool:
        try:
            async with self.download_lock:
                if not self.download_queue and not self.current_download:
                    return False

                if not self.current_download and self.download_queue:
                    self.current_download = self.download_queue.pop(0)

            if self.current_download:
                uid, hotkey, hash = self.current_download

                axon = self.metagraph.axons[uid]
                circuit_dir = os.path.join(self.temp_directory, hash)
                os.makedirs(circuit_dir, exist_ok=True)

                if await self.circuit_manager.download_files(axon, hash, circuit_dir):
                    if self.circuit_validator.validate_files(circuit_dir):
                        self.miner_states[hotkey] = NeuronState(
                            hotkey=hotkey,
                            uid=uid,
                            sota_relative_score=0.0,
                            proof_size=0,
                            response_time=0.0,
                            verification_result=False,
                            raw_accuracy=0.0,
                            hash=hash,
                        )
                        return True
                if os.path.exists(circuit_dir):
                    shutil.rmtree(circuit_dir)

                self.current_download = None
        except Exception as e:
            bt.logging.error(f"Error in download processing: {e}")
            traceback.print_exc()
            if self.current_download:
                uid, hotkey, hash = self.current_download
                circuit_dir = os.path.join(self.temp_directory, hash)
                if os.path.exists(circuit_dir):
                    shutil.rmtree(circuit_dir)
                self.current_download = None
        return False

    def queue_download(self, uid: int, hotkey: str, hash: str) -> None:
        self.download_queue.append((uid, hotkey, hash))
        self.download_complete.set()

    def get_current_download(self) -> Optional[Tuple[int, str, str]]:
        return self.current_download

    def clear_current_download(self) -> None:
        """Clear the current download without cleaning up the directory."""
        self.current_download = None

    def cleanup_circuit_dir(self, circuit_dir: str) -> None:
        """Clean up a circuit directory if it exists."""
        if os.path.exists(circuit_dir):
            bt.logging.info(f"Cleaning up circuit directory: {circuit_dir}")
            specific_dir = os.path.basename(circuit_dir)
            cleanup_temp_dir(specific_dir=specific_dir)

    def _evaluate_circuit(
        self,
        circuit_dir: str,
        circuit_uid: str,
        circuit_owner: str,
    ) -> bool:
        try:
            (
                sota_relative_score,
                proof_size,
                response_time,
                verification_result,
                accuracy,
            ) = self.circuit_evaluator.evaluate(circuit_dir)

            self.circuits_evaluated_count += 1

            if circuit_owner not in self.miner_states:
                self.miner_states[circuit_owner] = NeuronState(
                    hotkey=circuit_owner,
                    uid=int(circuit_uid),
                    sota_relative_score=0.0,
                    proof_size=0,
                    response_time=0.0,
                    verification_result=False,
                    raw_accuracy=0.0,
                    hash=os.path.basename(circuit_dir),
                )

            neuron_state = self.miner_states[circuit_owner]
            neuron_state.sota_relative_score = sota_relative_score
            neuron_state.proof_size = proof_size
            neuron_state.response_time = response_time
            neuron_state.verification_result = verification_result
            neuron_state.raw_accuracy = accuracy

            if verification_result:
                self.sota_manager.preserve_circuit(
                    circuit_dir=circuit_dir,
                    hotkey=circuit_owner,
                    uid=int(circuit_uid),
                    raw_accuracy=accuracy,
                    proof_size=proof_size,
                    response_time=response_time,
                    hash=os.path.basename(circuit_dir),
                    miner_states=self.miner_states,
                )

            self._update_competition_metrics()
            return verification_result
        except Exception as e:
            bt.logging.error(f"Error in competition thread cycle: {str(e)}")
            bt.logging.error(f"Stack trace: {traceback.format_exc()}")
            return False

    def _update_competition_metrics(self):
        """Calculates ranks and overall averages based on current miner states."""
        try:
            active_miners = [
                (k, v)
                for k, v in self.miner_states.items()
                if getattr(v, "verification_result", False)
                and getattr(v, "sota_relative_score", 0) > 0
            ]
            active_participants = len(active_miners)

            self.avg_raw_accuracy = 0.0
            self.avg_proof_size = 0.0
            self.avg_response_time = 0.0
            self.avg_verification_rate = 1.0

            if active_miners:
                sorted_by_accuracy = sorted(
                    active_miners,
                    key=lambda x: getattr(x[1], "raw_accuracy", 0),
                    reverse=True,
                )
                sorted_by_proof_size = sorted(
                    active_miners,
                    key=lambda x: getattr(x[1], "proof_size", float("inf")),
                )
                sorted_by_response_time = sorted(
                    active_miners,
                    key=lambda x: getattr(x[1], "response_time", float("inf")),
                )
                sorted_by_sota_score = sorted(
                    active_miners,
                    key=lambda x: getattr(x[1], "sota_relative_score", 0),
                    reverse=True,
                )

                max_rank = len(self.miner_states) + 1
                for _hotkey, state in self.miner_states.items():
                    state.rank_overall = max_rank
                    state.rank_accuracy = max_rank
                    state.rank_proof_size = max_rank
                    state.rank_response_time = max_rank

                for i, (h, state) in enumerate(sorted_by_sota_score):
                    state.rank_overall = i + 1
                for i, (h, state) in enumerate(sorted_by_accuracy):
                    state.rank_accuracy = i + 1
                for i, (h, state) in enumerate(sorted_by_proof_size):
                    state.rank_proof_size = i + 1
                for i, (h, state) in enumerate(sorted_by_response_time):
                    state.rank_response_time = i + 1

                self.avg_raw_accuracy = (
                    sum(getattr(m[1], "raw_accuracy", 0) for m in active_miners)
                    / active_participants
                )
                self.avg_proof_size = (
                    sum(getattr(m[1], "proof_size", 0) for m in active_miners)
                    / active_participants
                )
                self.avg_response_time = (
                    sum(getattr(m[1], "response_time", 0) for m in active_miners)
                    / active_participants
                )
                num_valid = len(active_miners)
                total_attempts = len(self.miner_states)
                self.avg_verification_rate = (
                    float(num_valid / total_attempts) if total_attempts else 1.0
                )

        except Exception as e:
            bt.logging.warning(f"Error updating competition ranks/averages: {e}")
            bt.logging.debug(f"Stack trace: {traceback.format_exc()}")

    def get_summary_for_logging(self) -> dict | None:
        """Prepares a summary dictionary for logging competition state to the backend."""
        try:
            comp_config = getattr(self, "config", None)
            if not comp_config:
                bt.logging.warning("Competition config not found for logging summary.")
                return None

            current_miner_states = getattr(self, "miner_states", {})
            if not current_miner_states:
                bt.logging.warning("No miner states found for logging summary.")
                return None

            avg_accuracy_val = getattr(self, "avg_raw_accuracy", 0.0)
            avg_proof_size_val = getattr(self, "avg_proof_size", 0.0)
            avg_response_time_val = getattr(self, "avg_response_time", 0.0)
            avg_verification_rate_val = getattr(self, "avg_verification_rate", 1.0)

            metrics_to_log = {
                "competition_id": self.competition_id,
                "name": comp_config.get("name", f"Competition_{self.competition_id}"),
                "status": "running",
                "accuracy_weight": float(
                    comp_config.get("evaluation", {})
                    .get("scoring_weights", {})
                    .get("accuracy", 0.4)
                ),
                "total_circuits_evaluated": int(
                    getattr(self, "circuits_evaluated_count", 0)
                ),
                "timestamp": int(time.time()),
                "active_participants": len(current_miner_states),
                "avg_accuracy": avg_accuracy_val,
                "avg_proof_size": avg_proof_size_val,
                "avg_response_time": avg_response_time_val,
                "avg_verification_rate": avg_verification_rate_val,
                "competitors": [
                    {
                        "hotkey": state.hotkey,
                        "uid": state.uid,
                        "sota_score": float(getattr(state, "sota_relative_score", 0.0)),
                        "proof_size": int(getattr(state, "proof_size", 0)),
                        "response_time": float(getattr(state, "response_time", 0.0)),
                        "verification_rate": (
                            1.0 if getattr(state, "verification_result", False) else 0.0
                        ),
                        "rank_overall": int(getattr(state, "rank_overall", 999)),
                        "raw_accuracy": float(getattr(state, "raw_accuracy", 0.0)),
                    }
                    for hotkey, state in current_miner_states.items()
                ],
            }
            return metrics_to_log

        except Exception as e:
            bt.logging.error(
                f"Error preparing competition summary for logging: {e}", exc_info=True
            )
            return None

    def set_validator_message_queues(
        self,
        validator_to_competition_queue: MPQueue,
        competition_to_validator_queue: MPQueue,
    ):
        self.competition_thread.set_validator_message_queues(
            validator_to_competition_queue, competition_to_validator_queue
        )
