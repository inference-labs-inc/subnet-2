from typing import Tuple

from constants import EPOCH_TEMPO


def get_current_epoch_info(current_block: int, netuid: int) -> Tuple[int, int, int]:
    """
    Calculates epoch information for the current block based on the Subtensor epoch logic.

    Args:
        current_block (int): The current block number.
        netuid (int): The netuid of the subnet.

    Returns:
        Tuple[int, int, int]: A tuple containing:
            - current_epoch (int): The current epoch number.
            - blocks_until_next_epoch (int): The number of blocks until the next epoch.
            - epoch_start_block (int): The starting block of the current epoch.
    """
    tempo_plus_one = EPOCH_TEMPO + 1
    adjusted_block = current_block + netuid + 1

    current_epoch = adjusted_block // tempo_plus_one
    remainder = adjusted_block % tempo_plus_one
    blocks_until_next_epoch = EPOCH_TEMPO - remainder
    epoch_start_block = current_block - (EPOCH_TEMPO - blocks_until_next_epoch)

    return current_epoch, blocks_until_next_epoch, epoch_start_block


def get_epoch_start_block(epoch: int, netuid: int) -> int:
    """
    Calculates the starting block of a given epoch.

    Args:
        epoch (int): The epoch number to get the start block for.
        netuid (int): The netuid of the subnet.

    Returns:
        int: The starting block of the specified epoch.
    """
    tempo_plus_one = EPOCH_TEMPO + 1
    return (epoch * tempo_plus_one) - (netuid + 1)
