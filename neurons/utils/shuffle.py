import bittensor as bt
import random
import hashlib
from typing import List

from .epoch import get_epoch_start_block
from constants import NUM_MINER_GROUPS


def get_shuffled_uids(
    current_epoch: int,
    last_shuffle_epoch: int,
    metagraph: bt.metagraph,
    subtensor: bt.subtensor,
    shuffled_uids: List[int] | None,
) -> tuple[List[int], int, int | None, str | None]:
    """
    Get the shuffled UIDs for the current epoch.

    Args:
        current_epoch (int): The current epoch number.
        last_shuffle_epoch (int): The last epoch where shuffling occurred.
        metagraph (bt.metagraph): The metagraph of the subnet.
        subtensor (bt.subtensor): The subtensor instance.
        shuffled_uids (List[int] | None): The current list of shuffled UIDs.

    Returns:
        tuple[List[int], int, int | None, str | None]: A tuple containing:
            - shuffled_uids (List[int]): The shuffled list of UIDs.
            - last_shuffle_epoch (int): The epoch number of the last shuffle.
            - seed_block_num (int | None): The block number used for the seed.
            - block_hash (str | None): The block hash used for the seed.
    """
    seed_block_num = None
    block_hash = None
    if last_shuffle_epoch < 0 or (current_epoch // NUM_MINER_GROUPS) > (
        last_shuffle_epoch // NUM_MINER_GROUPS
    ):
        bt.logging.info(f"Reshuffling miner UIDs for epoch {current_epoch}")
        last_shuffle_epoch = current_epoch

        cycle_start_epoch = (current_epoch // NUM_MINER_GROUPS) * NUM_MINER_GROUPS
        seed_block_num = get_epoch_start_block(cycle_start_epoch, metagraph.netuid)
        block_hash = subtensor.get_block_hash(seed_block_num)

        if not block_hash:
            bt.logging.warning(
                f"Could not get block hash for epoch {cycle_start_epoch}, using current epoch as seed"
            )
            seed = cycle_start_epoch
        else:
            seed = int(hashlib.sha256(block_hash.encode()).hexdigest(), 16)

        uids = list(range(len(metagraph.uids)))
        random.Random(seed).shuffle(uids)
        shuffled_uids = uids

    if shuffled_uids is None:
        shuffled_uids = list(range(len(metagraph.uids)))
        last_shuffle_epoch = current_epoch

    return shuffled_uids, last_shuffle_epoch, seed_block_num, block_hash
