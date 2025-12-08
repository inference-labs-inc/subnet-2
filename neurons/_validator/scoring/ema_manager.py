from __future__ import annotations
import torch
from rich.console import Console
from rich.table import Table
import bittensor as bt

from constants import (
    NUM_MINER_GROUPS,
    ONE_MINUTE,
    EMA_BOOST_FACTOR,
    EMA_ENABLED,
)
from utils.epoch import get_current_epoch_info
from utils.rate_limiter import with_rate_limit


class EMAManager:
    def __init__(self, scores: torch.Tensor, metagraph: bt.metagraph):
        self.scores = scores
        self.metagraph = metagraph
        self.last_ema_segment_per_uid = {}

    @with_rate_limit(period=ONE_MINUTE)
    def log_ema(
        self,
        current_epoch: int,
        blocks_until_next_epoch: int,
        active_group: int,
        boosted_group: int,
    ):
        """Logs a summary of the current EMA boosting status for miner groups."""
        table = Table(
            title=f"EMA Boost Status (Epoch: {current_epoch}, Blocks Until Next: "
            f"{blocks_until_next_epoch}, Active Group: {active_group}, Boosted Group: {boosted_group})"
        )
        table.add_column("Group ID", justify="center", style="cyan")
        table.add_column("Status", justify="center", style="magenta")

        for group_id in range(NUM_MINER_GROUPS):
            if group_id == boosted_group:
                status = "🚀"
            else:
                status = "📉"
            table.add_row(str(group_id), status)

        console = Console()
        console.print(table)

    def apply_ema_boost(self, shuffled_uids: list[int]):
        current_block = self.metagraph.subtensor.get_current_block()
        current_epoch, blocks_until_next_epoch, _ = get_current_epoch_info(
            current_block, self.metagraph.netuid
        )

        active_group = current_epoch % NUM_MINER_GROUPS
        boosted_group = (active_group + 1) % NUM_MINER_GROUPS
        self.log_ema(
            current_epoch, blocks_until_next_epoch, active_group, boosted_group
        )

        for i, uid in enumerate(shuffled_uids):
            if uid >= len(self.scores):
                bt.logging.warning(f"UID {uid} not found in scores for EMA boost")
                continue

            group = i % NUM_MINER_GROUPS
            if self.scores[uid] is not None:
                last_ema_epoch = self.last_ema_segment_per_uid.get(uid, -1)

                if last_ema_epoch != current_epoch and EMA_ENABLED:
                    if group == boosted_group:
                        self.scores[uid] = self.scores[uid] * EMA_BOOST_FACTOR
                    else:
                        self.scores[uid] = self.scores[uid] * 0.99

                    self.last_ema_segment_per_uid[uid] = current_epoch
