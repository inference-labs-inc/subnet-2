from __future__ import annotations
from collections import deque
from dataclasses import dataclass, field
import json
import os
import threading
import torch
import bittensor as bt
from constants import (
    PERFORMANCE_CURVE_POWER,
    PERFORMANCE_MIN_SAMPLES,
    PERFORMANCE_WINDOW_SIZE,
    WEIGHT_RATE_LIMIT,
    WEIGHTS_VERSION,
    ONE_MINUTE,
)
from _validator.utils.logging import log_weights
from _validator.utils.proof_of_weights import ProofOfWeightsItem

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from _validator.scoring.score_manager import ScoreManager


class PerformanceTracker:
    def __init__(
        self,
        window_size: int = PERFORMANCE_WINDOW_SIZE,
        persistence_path: str | None = None,
    ):
        self.window_size = window_size
        self.windows: dict[int, deque[bool]] = {}
        self._lock = threading.Lock()
        self.persistence_path = persistence_path
        self._load()

    def record(self, uid: int, success: bool) -> None:
        with self._lock:
            if uid not in self.windows:
                self.windows[uid] = deque(maxlen=self.window_size)
            self.windows[uid].append(success)

    def success_rate(self, uid: int) -> float:
        with self._lock:
            if uid not in self.windows or len(self.windows[uid]) == 0:
                return 0.0
            return sum(self.windows[uid]) / len(self.windows[uid])

    def sample_count(self, uid: int) -> int:
        with self._lock:
            if uid not in self.windows:
                return 0
            return len(self.windows[uid])

    def snapshot(self) -> dict[int, tuple[float, int]]:
        with self._lock:
            return {
                uid: (sum(w) / len(w) if len(w) > 0 else 0.0, len(w))
                for uid, w in self.windows.items()
            }

    def save(self) -> None:
        if not self.persistence_path:
            return
        try:
            os.makedirs(os.path.dirname(self.persistence_path), exist_ok=True)
            with self._lock:
                data = {str(uid): list(window) for uid, window in self.windows.items()}
            tmp_path = self.persistence_path + ".tmp"
            with open(tmp_path, "w") as f:
                json.dump(data, f)
            os.replace(tmp_path, self.persistence_path)
        except Exception as e:
            bt.logging.error(f"Failed to save performance tracker: {e}")

    def _load(self) -> None:
        if not self.persistence_path or not os.path.exists(self.persistence_path):
            return
        try:
            with open(self.persistence_path, "r") as f:
                data = json.load(f)
            for uid_str, results in data.items():
                self.windows[int(uid_str)] = deque(
                    (bool(v) for v in results), maxlen=self.window_size
                )
            bt.logging.info(f"Loaded performance tracker with {len(self.windows)} UIDs")
        except Exception as e:
            bt.logging.error(f"Failed to load performance tracker: {e}")


@dataclass
class WeightsManager:
    """
    Manages weight setting for the validator.

    Attributes:
        subtensor (bt.Subtensor): The Bittensor subtensor instance.
        metagraph (bt.Metagraph): The Bittensor metagraph instance.
        wallet (bt.Wallet): The Bittensor wallet instance.
        user_uid (int): The unique identifier of the validator.
        weights (Optional[torch.Tensor]): The current weights tensor.
        last_update_weights_block (int): The last block number when weights were updated.
        proof_of_weights_queue (List[ProofOfWeightsItem]): Queue for proof of weights items.
        score_manager: Optional ScoreManager for accessing EMA manager and shuffled UIDs.
    """

    subtensor: bt.Subtensor
    metagraph: bt.Metagraph
    wallet: bt.Wallet
    user_uid: int
    last_update_weights_block: int = 0
    proof_of_weights_queue: list[ProofOfWeightsItem] = field(default_factory=list)
    score_manager: "ScoreManager" = None
    performance_tracker: PerformanceTracker = field(default=None)

    def __post_init__(self):
        if self.performance_tracker is None:
            tracker_path = os.path.join(
                os.path.expanduser("~"),
                ".bittensor",
                "subnet-2",
                "performance_tracker.json",
            )
            self.performance_tracker = PerformanceTracker(persistence_path=tracker_path)

    def set_weights(self, netuid, wallet, uids, weights, version_key):
        return self.subtensor.set_weights(
            netuid=netuid,
            wallet=wallet,
            uids=uids,
            weights=weights,
            wait_for_inclusion=False,
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

    def _compute_performance_weights(self, scores: torch.Tensor) -> torch.Tensor:
        n = self.metagraph.n
        weights = torch.zeros(n)
        snap = self.performance_tracker.snapshot()
        exploration_floor = 1.0 / n

        for uid in range(n):
            rate, count = snap.get(uid, (0.0, 0))
            if count >= PERFORMANCE_MIN_SAMPLES:
                weights[uid] = rate**PERFORMANCE_CURVE_POWER
            elif uid < len(scores) and scores[uid] > 0:
                weights[uid] = exploration_floor

        total = weights.sum()
        if total > 0:
            weights = weights / total

        return weights

    def update_weights(self, scores: torch.Tensor) -> bool:
        """Updates the weights based on the given scores and sets them on the chain."""
        should_update, message = self.should_update_weights()
        if not should_update:
            bt.logging.info(message)
            return True

        bt.logging.info("Updating weights")

        weights = self._compute_performance_weights(scores)

        snap = self.performance_tracker.snapshot()
        tracked = {
            uid: rate
            for uid, (rate, count) in snap.items()
            if count >= PERFORMANCE_MIN_SAMPLES
        }
        if tracked:
            top = sorted(tracked.items(), key=lambda x: x[1], reverse=True)[:5]
            bt.logging.info(
                f"Performance tracker: {len(tracked)} UIDs tracked, "
                f"top 5: {[(uid, f'{rate:.2%}') for uid, rate in top]}"
            )

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
                self.performance_tracker.save()
                return True
            return False

        except Exception as e:
            bt.logging.error(f"Failed to set weights on chain with exception: {e}")
            return False
