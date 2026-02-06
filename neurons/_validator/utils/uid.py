from collections.abc import Generator, Iterable
import os
import bittensor as bt
import torch
import ipaddress

from constants import VALIDATOR_STAKE_THRESHOLD, MAINNET_TESTNET_UIDS, DEFAULT_NETUID


def get_target_uids() -> set[int] | None:
    target_uids_str = os.environ.get("TARGET_UIDS", "")
    if not target_uids_str:
        return None
    return {int(uid.strip()) for uid in target_uids_str.split(",") if uid.strip()}


def is_valid_ip(ip: str) -> bool:
    try:
        address = ipaddress.IPv4Address(ip)
        return address.is_global and not address.is_multicast
    except ValueError:
        return False


def get_queryable_uids(metagraph: bt.Metagraph) -> Generator[int, None, None]:
    """
    Returns the uids of the miners that are queryable
    """
    uids = metagraph.uids.tolist()
    stake_threshold = VALIDATOR_STAKE_THRESHOLD
    if metagraph.netuid in [
        i[1] for i in MAINNET_TESTNET_UIDS if i[0] == DEFAULT_NETUID
    ]:
        stake_threshold = 1e19
    total_stake = (
        torch.tensor(metagraph.total_stake, dtype=torch.float32)
        if not isinstance(metagraph.total_stake, torch.Tensor)
        else metagraph.total_stake
    )
    total_stake = total_stake[uids]
    queryable_flags: Iterable[bool] = (
        (total_stake < stake_threshold)
        & torch.tensor([is_valid_ip(metagraph.axons[i].ip) for i in uids])
    ).tolist()
    target_uids = get_target_uids()
    for uid, is_queryable in zip(uids, queryable_flags):
        if is_queryable:
            if target_uids is None or uid in target_uids:
                yield uid
