from __future__ import annotations
from dataclasses import dataclass, field
import torch
import bittensor as bt
from constants import (
    WEIGHT_RATE_LIMIT,
    WEIGHTS_VERSION,
    ONE_MINUTE,
)
from _validator.utils.logging import log_weights
from _validator.utils.proof_of_weights import ProofOfWeightsItem

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from _validator.scoring.score_manager import ScoreManager


@dataclass
class WeightsManager:
    """
    Manages weight setting for the validator.

    Attributes:
        subtensor (bt.subtensor): The Bittensor subtensor instance.
        metagraph (bt.metagraph): The Bittensor metagraph instance.
        wallet (bt.wallet): The Bittensor wallet instance.
        user_uid (int): The unique identifier of the validator.
        weights (Optional[torch.Tensor]): The current weights tensor.
        last_update_weights_block (int): The last block number when weights were updated.
        proof_of_weights_queue (List[ProofOfWeightsItem]): Queue for proof of weights items.
        score_manager: Optional ScoreManager for accessing EMA manager and shuffled UIDs.
    """

    subtensor: bt.subtensor
    metagraph: bt.metagraph
    wallet: bt.wallet
    user_uid: int
    last_update_weights_block: int = 0
    proof_of_weights_queue: list[ProofOfWeightsItem] = field(default_factory=list)
    score_manager: "ScoreManager" = None

    def set_weights(self, netuid, wallet, uids, weights, version_key):
        return self.subtensor.set_weights(
            netuid=netuid,
            wallet=wallet,
            uids=uids,
            weights=weights,
            wait_for_inclusion=True,
            version_key=version_key,
        )

    def should_update_weights(self) -> tuple[bool, str]:
        """Check if weights should be updated based on rate limiting and epoch timing."""
        blocks_since_last_update = self.subtensor.blocks_since_last_update(
            self.metagraph.netuid, self.user_uid
        )
        if blocks_since_last_update < WEIGHT_RATE_LIMIT:
            blocks_until_update = WEIGHT_RATE_LIMIT - blocks_since_last_update
            minutes_until_update = round((blocks_until_update * 12) / ONE_MINUTE, 1)
            return (
                False,
                f"Next weight update in {blocks_until_update} blocks "
                f"(approximately {minutes_until_update:.1f} minutes)",
            )

        return True, ""

    def update_weights(self, scores: torch.Tensor) -> bool:
        """Updates the weights based on the given scores and sets them on the chain."""
        should_update, message = self.should_update_weights()
        if not should_update:
            bt.logging.info(message)
            return True

        bt.logging.info("Updating weights")

        weights = torch.zeros(self.metagraph.n)
        nonzero_indices = scores.nonzero()
        bt.logging.debug(
            f"Weights: {weights}, Nonzero indices: {nonzero_indices}, Scores: {scores}"
        )
        if nonzero_indices.sum() > 0:
            weights[nonzero_indices] = scores[nonzero_indices]

        owner_hotkey = self.subtensor.get_subnet_owner_hotkey(self.metagraph.netuid)
        if owner_hotkey and owner_hotkey in self.metagraph.hotkeys:
            owner_uid = self.metagraph.hotkeys.index(owner_hotkey)
            weights = weights * 0.2
            weights[owner_uid] = 0.8

        try:
            success, message = self.set_weights(
                netuid=self.metagraph.netuid,
                wallet=self.wallet,
                uids=self.metagraph.uids.tolist(),
                weights=weights.tolist(),
                version_key=WEIGHTS_VERSION,
            )

            if message:
                bt.logging.info(f"Set weights message: {message}")

            if success:
                bt.logging.success("Weights were set successfully")
                log_weights(weights)
                self.last_update_weights_block = int(self.metagraph.block.item())
                return True
            return False

        except Exception as e:
            bt.logging.error(f"Failed to set weights on chain with exception: {e}")
            return False
