from __future__ import annotations
import torch
import bittensor as bt

from _validator.utils.proof_of_weights import ProofOfWeightsItem
from constants import (
    ONE_MINUTE,
)
from execution_layer.verified_model_session import VerifiedModelSession
from deployment_layer.circuit_store import circuit_store
from _validator.models.request_type import RequestType
from utils.rate_limiter import with_rate_limit
from _validator.utils.logging import log_scores


class ProofOfWeightsManager:
    def __init__(self, metagraph: bt.metagraph, scores: torch.Tensor):
        self.metagraph = metagraph
        self.scores = scores
        self.proof_of_weights_queue = []
        self.last_processed_queue_step = -1

    def _update_scores_from_witness(
        self, proof_of_weights_items: list[ProofOfWeightsItem], model_id: str
    ):
        pow_circuit = circuit_store.get_circuit(model_id)
        bt.logging.info(
            f"Processing PoW witness generation for {len(proof_of_weights_items)} items using {str(pow_circuit)}"
        )
        if not pow_circuit:
            raise ValueError(
                f"Proof of weights circuit not found for model ID: {model_id}"
            )

        inputs = pow_circuit.input_handler(
            RequestType.RWR, ProofOfWeightsItem.to_dict_list(proof_of_weights_items)
        )
        session = VerifiedModelSession(inputs, pow_circuit)
        try:
            witness = session.generate_witness(return_content=True)
            bt.logging.success(
                f"Witness for {str(pow_circuit)} generated successfully."
            )
        except Exception as e:
            bt.logging.error(f"Error generating witness: {e}")
            return

        witness_list = witness if isinstance(witness, list) else list(witness.values())
        self._process_witness_results(witness_list, pow_circuit.settings["scaling"])
        session.end()

    def _process_witness_results(self, witness: list, scaling: int):
        """Process the results from the witness."""
        scores = torch.div(
            torch.tensor([float(w) for w in witness[1:257]]), scaling
        ).tolist()
        miner_uids = [int(float(w)) for w in witness[513:769]]

        bt.logging.debug(
            f"Proof of weights scores: {scores} for miner UIDs: {miner_uids}, existing scores: {self.scores}"
        )

        for uid, score in zip(miner_uids, scores):
            if uid < 0 or uid >= len(self.scores):
                continue
            self.scores[uid] = float(score)
            bt.logging.debug(f"Updated score for UID {uid}: {score}")

        log_scores(self.scores)

    def update_pow_queue(self, new_items: list[ProofOfWeightsItem]):
        if not new_items:
            return

        self.proof_of_weights_queue.extend(new_items)
        self.log_pow_queue_status()

    @with_rate_limit(period=ONE_MINUTE)
    def log_pow_queue_status(self):
        bt.logging.info(f"PoW Queue Status: {len(self.proof_of_weights_queue)} items")

    def process_pow_queue(self, model_id: str) -> bool:
        """Process items in the proof of weights queue for a specific model."""
        if (
            len(self.proof_of_weights_queue) < 256
            or len(self.proof_of_weights_queue) % 256 != 0
        ):
            return False

        current_step = len(self.proof_of_weights_queue) >> 8
        if current_step == self.last_processed_queue_step:
            return False

        pow_circuit = circuit_store.get_circuit(model_id)
        if not pow_circuit:
            bt.logging.error(f"Circuit not found for model ID: {model_id}")
            return False

        items_to_process = self.proof_of_weights_queue[-256:]
        self._update_scores_from_witness(items_to_process, model_id)
        self.last_processed_queue_step = current_step

        return True

    def clear_proof_of_weights_queue(self):
        """Clear the proof of weights queue."""
        self.proof_of_weights_queue = []

    def get_pow_queue(self) -> list[ProofOfWeightsItem]:
        """Get the current proof of weights queue."""
        return self.proof_of_weights_queue

    def remove_processed_items(self, count: int):
        if count <= 0:
            return
        self.proof_of_weights_queue = self.proof_of_weights_queue[count:]
