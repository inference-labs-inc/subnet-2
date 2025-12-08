from __future__ import annotations
import torch
import bittensor as bt

from _validator.models.miner_response import MinerResponse
from _validator.utils.logging import log_scores
from _validator.utils.proof_of_weights import ProofOfWeightsItem
from _validator.utils.uid import get_queryable_uids
from constants import (
    SINGLE_PROOF_OF_WEIGHTS_MODEL_ID,
    NUM_MINER_GROUPS,
    RESET_PENALTY_ENABLED,
)
from execution_layer.circuit import CircuitEvaluationItem
from utils.epoch import get_current_epoch_info
from _validator.competitions.competition import Competition
from _validator.scoring.ema_manager import EMAManager
from _validator.scoring.pow_manager import ProofOfWeightsManager
from _validator.scoring.reset_manager import ResetManager
from utils.shuffle import get_shuffled_uids


class ScoreManager:
    """Manages the scoring of miners."""

    def __init__(
        self,
        metagraph: bt.metagraph,
        user_uid: int,
        score_path: str,
        competition: Competition | None = None,
    ):
        """
        Initialize the ScoreManager.

        Args:
            metagraph: The metagraph of the subnet.
            user_uid: The UID of the current user.
        """
        self.metagraph = metagraph
        self.user_uid = user_uid
        self.score_path = score_path
        self.scores = self.init_scores()
        self.competition = competition
        self.shuffled_uids = None
        self.last_shuffle_epoch = -1
        self.seed_block_num = None
        self.block_hash = None

        self.pow_manager = ProofOfWeightsManager(self.metagraph, self.scores)
        self.reset_manager = ResetManager(self.metagraph)
        self.ema_manager = EMAManager(self.scores, self.metagraph)

    def init_scores(self) -> torch.Tensor:
        """Initialize or load existing scores."""
        bt.logging.info("Initializing validation weights")
        try:
            scores = torch.load(self.score_path, weights_only=True)
        except FileNotFoundError:
            scores = self._create_initial_scores()
        except Exception as e:
            bt.logging.error(f"Error loading scores: {e}")
            scores = self._create_initial_scores()

        bt.logging.success("Successfully initialized scores")
        log_scores(scores)
        return scores

    def _create_initial_scores(self) -> torch.Tensor:
        """Create initial scores based on metagraph data."""
        return torch.zeros(len(self.metagraph.uids), dtype=torch.float32)

    def _get_safe_score(self, uid: int) -> float:
        """Safely get score for a UID, returning 0 if out of bounds."""
        if uid >= len(self.scores):
            bt.logging.warning(
                f"UID {uid} is out of bounds for scores tensor of size {len(self.scores)}"
            )
            return 0.0
        return float(self.scores[uid])

    def sync_scores_uids(self, uids: list[int]):
        """
        If there are more uids than scores, add more weights.
        """
        if len(uids) > len(self.scores):
            bt.logging.trace(
                f"Scores length: {len(self.scores)}, UIDs length: {len(uids)}. Adding more weights"
            )
            size_difference = len(uids) - len(self.scores)
            new_scores = torch.zeros(size_difference, dtype=torch.float32)
            queryable_uids = set(get_queryable_uids(self.metagraph))
            new_scores = new_scores * torch.Tensor(
                [
                    uid in queryable_uids
                    for uid in self.metagraph.uids[len(self.scores) :]
                ]
            )
            self.scores = torch.cat((self.scores, new_scores))
            self.reset_manager.reset_tracker = [True for _ in range(len(uids))]

    def _try_store_scores(self):
        """Attempt to store scores to disk."""
        try:
            torch.save(self.scores, self.score_path)
            bt.logging.info(f"Saved scores to {self.score_path}")
        except Exception as e:
            bt.logging.error(f"Error storing scores: {e}")

    def process_non_queryable_scores(self, queryable_uids: set[int], max_score: float):
        """
        Decay scores for non-queryable UIDs.
        """
        for uid in range(len(self.scores)):
            if uid not in queryable_uids:
                hotkey = self.metagraph.hotkeys[uid]
                if not (self.competition and hotkey in self.competition.miner_states):
                    self.scores[uid] = 0
                elif self.competition and hotkey in self.competition.miner_states:
                    pow_item = ProofOfWeightsItem.for_competition(
                        uid=uid,
                        maximum_score=max_score,
                        competition_score=self.competition.miner_states[
                            hotkey
                        ].sota_relative_score,
                        block_number=self.metagraph.block.item(),
                        validator_uid=self.user_uid,
                    )
                    self.pow_manager.update_pow_queue([pow_item])

    def update_single_score(
        self,
        response: MinerResponse,
        queryable_uids: set[int] | None = None,
    ) -> None:
        """
        Update the score for a single miner based on their response.

        Args:
            response (MinerResponse): The processed response from a miner.
            queryable_uids: Optional pre-computed set of queryable UIDs.
        """
        if queryable_uids is None:
            queryable_uids = set(get_queryable_uids(self.metagraph))

        circuit = response.circuit

        current_block = self.metagraph.subtensor.get_current_block()
        current_epoch, _, _ = get_current_epoch_info(
            current_block, self.metagraph.netuid
        )

        (
            self.shuffled_uids,
            self.last_shuffle_epoch,
            new_seed_block_num,
            new_block_hash,
        ) = get_shuffled_uids(
            current_epoch,
            self.last_shuffle_epoch,
            self.metagraph,
            self.metagraph.subtensor,
            self.shuffled_uids,
        )
        if new_seed_block_num is not None:
            self.seed_block_num = new_seed_block_num
        if new_block_hash is not None:
            self.block_hash = new_block_hash

        uid_index = self.shuffled_uids.index(response.uid)
        miner_group = uid_index % NUM_MINER_GROUPS

        miner_missed_reset = self.reset_manager.miner_missed_reset(
            response.uid,
            miner_group,
            current_epoch,
            self.last_shuffle_epoch,
        )
        self.reset_manager.set_reset_status(response.uid, miner_missed_reset)
        self.reset_manager.log_reset_tracker(
            self.shuffled_uids, self.seed_block_num, self.block_hash
        )
        if miner_missed_reset and RESET_PENALTY_ENABLED:
            bt.logging.warning(
                f"Miner {response.uid} missed required reset submission, marking as unverified"
            )
            response.verification_result = False

        evaluation_data = CircuitEvaluationItem(
            circuit=circuit,
            uid=response.uid,
            minimum_response_time=circuit.evaluation_data.minimum_response_time,
            proof_size=response.proof_size,
            response_time=response.response_time,
            score=self._get_safe_score(response.uid),
            verification_result=response.verification_result,
        )
        circuit.evaluation_data.update(evaluation_data)

        max_score = 1 / len(self.scores)
        self.process_non_queryable_scores(queryable_uids, max_score)

        pow_item = ProofOfWeightsItem.from_miner_response(
            response,
            max_score,
            self._get_safe_score(response.uid),
            circuit.evaluation_data.maximum_response_time,
            circuit.evaluation_data.minimum_response_time,
            self.metagraph.block.item(),
            self.user_uid,
            0,
        )

        self.pow_manager.update_pow_queue([pow_item])

        if self.pow_manager.process_pow_queue(SINGLE_PROOF_OF_WEIGHTS_MODEL_ID):
            self._try_store_scores()
            log_scores(self.scores)

    def get_pow_manager(self) -> ProofOfWeightsManager:
        """Get the proof of weights manager."""
        return self.pow_manager
