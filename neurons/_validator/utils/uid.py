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
    result = set()
    invalid_tokens = []
    for token in target_uids_str.split(","):
        token = token.strip()
        if not token:
            continue
        try:
            result.add(int(token))
        except ValueError:
            invalid_tokens.append(token)
    if invalid_tokens:
        raise ValueError(
            f"Invalid TARGET_UIDS tokens: {invalid_tokens}. "
            f"Original value: '{target_uids_str}'. Expected comma-separated integers."
        )
    return result if result else None


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
    target_uids = get_target_uids()
    if target_uids:
        for uid in uids:
            if uid in target_uids:
                yield uid
        return
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
    for uid, is_queryable in zip(uids, queryable_flags):
        if is_queryable:
            yield uid
