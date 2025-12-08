import os
import json
import time
import shutil
import bittensor as bt
from ..models.sota import SotaState
from ..models.neuron import NeuronState
import traceback
from _validator.utils.logging import log_sota_scores


class SotaManager:
    def __init__(self, sota_directory: str):
        self.sota_directory = sota_directory
        self.sota_state_path = os.path.join(sota_directory, "sota_state.json")
        self.miner_states_path = os.path.join(sota_directory, "miner_states.json")
        self.sota_state = self._load_state()

        try:
            competition_dir = os.path.dirname(sota_directory)
            with open(os.path.join(competition_dir, "competition_config.json")) as f:
                config = json.load(f)
                self.weights = config["evaluation"]["scoring_weights"]
        except Exception as e:
            bt.logging.error(f"Error loading scoring weights, using defaults: {e}")
            self.weights = {"accuracy": 0.95, "proof_size": 0.0, "response_time": 0.05}

    def _load_state(self) -> SotaState:
        try:
            if os.path.exists(self.sota_state_path):
                with open(self.sota_state_path, "r") as f:
                    data = json.load(f)
                    return SotaState(**data)
            return SotaState()
        except Exception as e:
            bt.logging.error(f"Error loading SOTA state: {e}")
            return SotaState()

    def _save_state(self):
        try:
            with open(self.sota_state_path, "w") as f:
                json.dump(self.sota_state.__dict__, f, indent=4)
        except Exception as e:
            bt.logging.error(f"Error saving SOTA state: {e}")

    def save_miner_states(self, miner_states: dict[str, NeuronState]):
        try:
            serialized_states = {k: v.__dict__ for k, v in miner_states.items()}
            with open(self.miner_states_path, "w") as f:
                json.dump(serialized_states, f, indent=4)
        except Exception as e:
            bt.logging.error(f"Error saving miner states: {e}")

    def load_miner_states(self) -> dict[str, NeuronState]:
        try:
            if os.path.exists(self.miner_states_path):
                with open(self.miner_states_path, "r") as f:
                    data = json.load(f)
                    return {k: NeuronState(**v) for k, v in data.items()}
            return {}
        except Exception as e:
            bt.logging.error(f"Error loading miner states: {e}")
            return {}

    def calculate_score(
        self,
        accuracy: float,
        proof_size: float,
        response_time: float,
        reference_states: dict[str, NeuronState] = None,
    ) -> float:
        if reference_states:
            best_accuracy = max(
                state.raw_accuracy for state in reference_states.values()
            )
            min_proof_size = min(
                state.proof_size
                for state in reference_states.values()
                if state.proof_size > 0
            )
            min_response_time = min(
                state.response_time
                for state in reference_states.values()
                if state.response_time > 0
            )
        else:
            best_accuracy = (
                self.sota_state.raw_accuracy
                if self.sota_state.raw_accuracy > 0
                else accuracy
            )
            min_proof_size = (
                self.sota_state.proof_size
                if self.sota_state.proof_size > 0
                else proof_size
            )
            min_response_time = (
                self.sota_state.response_time
                if self.sota_state.response_time > 0
                else response_time
            )

        accuracy_score = accuracy / best_accuracy if best_accuracy > 0 else 1
        proof_size_score = min_proof_size / proof_size if proof_size > 0 else 0
        response_time_score = (
            min_response_time / response_time if response_time > 0 else 0
        )

        return (
            accuracy_score * self.weights["accuracy"]
            + proof_size_score * self.weights["proof_size"]
            + response_time_score * self.weights["response_time"]
        )

    def preserve_circuit(
        self,
        circuit_dir: str,
        hotkey: str,
        uid: int,
        raw_accuracy: float,
        proof_size: float,
        response_time: float,
        hash: str,
        miner_states: dict[str, NeuronState],
    ):
        """Preserves the circuit if it represents a new SOTA based on raw performance."""
        try:
            current_score = self.calculate_score(
                raw_accuracy, proof_size, response_time
            )
            sota_score = (
                self.calculate_score(
                    self.sota_state.raw_accuracy,
                    self.sota_state.proof_size,
                    self.sota_state.response_time,
                )
                if self.sota_state.hash
                else 0
            )

            is_new_sota = current_score > sota_score * 1.02

            if is_new_sota:
                sota_circuit_dir = os.path.join(self.sota_directory, "circuit")
                if os.path.exists(sota_circuit_dir):
                    shutil.rmtree(sota_circuit_dir)
                shutil.copytree(circuit_dir, sota_circuit_dir)

                self.sota_state = SotaState(
                    sota_relative_score=1.0,
                    hash=hash,
                    hotkey=hotkey,
                    uid=uid,
                    proof_size=proof_size,
                    response_time=response_time,
                    timestamp=int(time.time()),
                    raw_accuracy=raw_accuracy,
                )
                self._save_state()

                bt.logging.success(
                    f"New SOTA achieved by {hotkey}! "
                    f"Score: {current_score:.4f}, "
                    f"Raw Accuracy: {raw_accuracy:.4f}, "
                    f"Proof Size: {proof_size:.0f} bytes, "
                    f"Response Time: {response_time:.4f}s"
                )
            else:
                bt.logging.debug(
                    f"Submission from {hotkey} did not improve SOTA. "
                    f"Score: {current_score:.4f} vs SOTA: {sota_score:.4f}"
                )

        except Exception as e:
            bt.logging.error(f"Error preserving SOTA circuit: {e}")
            bt.logging.error(traceback.format_exc())
        finally:
            self.recalculate_miner_scores(miner_states)

    def recalculate_miner_scores(self, miner_states: dict[str, NeuronState]) -> None:
        """Recalculate all miner scores using a strict ranking system."""
        try:
            valid_miners = {
                hotkey: state
                for hotkey, state in miner_states.items()
                if state.verification_result and state.raw_accuracy > 0
            }

            if not valid_miners:
                bt.logging.info("No valid miners to rank")
                return

            performance_scores = []
            for hotkey, state in valid_miners.items():
                score = self.calculate_score(
                    state.raw_accuracy,
                    state.proof_size,
                    state.response_time,
                    reference_states=valid_miners,
                )
                performance_scores.append((hotkey, score))

            performance_scores.sort(key=lambda x: x[1], reverse=True)
            max_score = (
                max(score for _, score in performance_scores)
                if performance_scores
                else 1
            )

            for hotkey, score in performance_scores:
                miner_states[hotkey].sota_relative_score = score / max_score

            for hotkey, state in miner_states.items():
                if hotkey not in [h for h, _ in performance_scores]:
                    state.sota_relative_score = 0.0

            log_sota_scores(performance_scores, miner_states, max_score)
            self.save_miner_states(miner_states)

        except Exception as e:
            bt.logging.error(f"Error recalculating miner scores: {e}")
            bt.logging.error(traceback.format_exc())

    @property
    def current_state(self) -> SotaState:
        return self.sota_state
