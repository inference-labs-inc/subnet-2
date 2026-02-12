from __future__ import annotations
from collections import deque
from dataclasses import dataclass, field
import json
import os
import threading
import torch
import bittensor as bt
from constants import (
    CIRCUIT_TIMEOUT_SECONDS,
    PERFORMANCE_CURVE_POWER,
    PERFORMANCE_MIN_SAMPLES,
    PERFORMANCE_RESCHEDULE_PENALTY,
    PERFORMANCE_RESPONSE_TIME_WEIGHT,
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
    RESPONSE_TIME_PERCENTILE = 0.95
    ADAPTIVE_TIMEOUT_MULTIPLIER = 2.0
    ADAPTIVE_TIMEOUT_MIN_SAMPLES = 50

    def __init__(
        self,
        window_size: int = PERFORMANCE_WINDOW_SIZE,
        persistence_path: str | None = None,
    ):
        self.window_size = window_size
        self.windows: dict[int, deque[tuple[bool, float]]] = {}
        self._lock = threading.Lock()
        self.persistence_path = persistence_path
        self._load()

    def _success_times(self) -> list[float]:
        times = []
        for w in self.windows.values():
            for success, rt in w:
                if success and rt > 0:
                    times.append(rt)
        return times

    def _percentile_time(self, times: list[float]) -> float:
        if not times:
            return CIRCUIT_TIMEOUT_SECONDS
        times.sort()
        idx = min(int(len(times) * self.RESPONSE_TIME_PERCENTILE), len(times) - 1)
        return times[idx]

    def _reference_time(self) -> float:
        return max(self._percentile_time(self._success_times()), 1.0)

    def _score(self, success: bool, response_time_sec: float, ref: float) -> float:
        if not success:
            return 0.0
        time_factor = max(0.0, 1.0 - (response_time_sec / ref))
        return PERFORMANCE_RESPONSE_TIME_WEIGHT * time_factor + (
            1.0 - PERFORMANCE_RESPONSE_TIME_WEIGHT
        )

    def record(
        self,
        uid: int,
        success: bool,
        response_time_sec: float = 0.0,
    ) -> None:
        with self._lock:
            if uid not in self.windows:
                self.windows[uid] = deque(maxlen=self.window_size)
            self.windows[uid].append((success, response_time_sec))

    def record_reschedule(self, uid: int) -> None:
        with self._lock:
            if uid not in self.windows:
                self.windows[uid] = deque(maxlen=self.window_size)
            self.windows[uid].append((False, PERFORMANCE_RESCHEDULE_PENALTY))

    def _uid_rate(self, w: deque[tuple[bool, float]], ref: float) -> float:
        if not w:
            return 0.0
        total = 0.0
        for success, rt in w:
            if rt == PERFORMANCE_RESCHEDULE_PENALTY:
                total += PERFORMANCE_RESCHEDULE_PENALTY
            else:
                total += self._score(success, rt, ref)
        return max(0.0, total / len(w))

    def success_rate(self, uid: int) -> float:
        with self._lock:
            if uid not in self.windows or len(self.windows[uid]) == 0:
                return 0.0
            ref = self._reference_time()
            return self._uid_rate(self.windows[uid], ref)

    def sample_count(self, uid: int) -> int:
        with self._lock:
            if uid not in self.windows:
                return 0
            return len(self.windows[uid])

    def adaptive_timeout(self) -> float:
        with self._lock:
            times = self._success_times()
            if len(times) < self.ADAPTIVE_TIMEOUT_MIN_SAMPLES:
                return CIRCUIT_TIMEOUT_SECONDS
            return min(
                self._percentile_time(times) * self.ADAPTIVE_TIMEOUT_MULTIPLIER,
                CIRCUIT_TIMEOUT_SECONDS,
            )

    def snapshot(self) -> dict[int, tuple[float, int]]:
        with self._lock:
            ref = self._reference_time()
            return {
                uid: (self._uid_rate(w, ref), len(w)) for uid, w in self.windows.items()
            }

    def miner_capacities(self, max_capacity: int) -> dict[int, int]:
        with self._lock:
            ref = self._reference_time()
            capacities = {}
            for uid, w in self.windows.items():
                count = len(w)
                if count < PERFORMANCE_MIN_SAMPLES:
                    capacities[uid] = 1
                    continue
                rate = self._uid_rate(w, ref)
                confidence = min(count / 50.0, 1.0)
                raw = 1 + (max_capacity - 1) * rate * confidence
                capacities[uid] = max(1, int(raw))
            return capacities

    def reset_uid(self, uid: int) -> None:
        with self._lock:
            if uid in self.windows:
                del self.windows[uid]

    def save(self) -> None:
        if not self.persistence_path:
            return
        try:
            os.makedirs(os.path.dirname(self.persistence_path), exist_ok=True)
            with self._lock:
                data = {
                    str(uid): [[s, rt] for s, rt in window]
                    for uid, window in self.windows.items()
                }
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
                entries = []
                for v in results:
                    if isinstance(v, list) and len(v) == 2:
                        entries.append((bool(v[0]), float(v[1])))
                    else:
                        score = float(v)
                        entries.append((score > 0, 0.0))
                self.windows[int(uid_str)] = deque(entries, maxlen=self.window_size)
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
            wait_for_inclusion=True,
            wait_for_finalization=True,
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
            adaptive_to = self.performance_tracker.adaptive_timeout()
            bt.logging.info(
                f"Performance tracker: {len(tracked)} UIDs tracked, "
                f"adaptive timeout: {adaptive_to:.1f}s, "
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
