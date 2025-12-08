import base64
import json
import os
from typing import Optional, TYPE_CHECKING

import bittensor as bt
import requests
import torch
from requests.adapters import HTTPAdapter
from urllib3.util.retry import Retry

if TYPE_CHECKING:
    from _validator.models.miner_response import MinerResponse

LOGGING_URL = os.getenv(
    "SUBNET_2_LOGGING_URL",
    "https://sn2-api.inferencelabs.com/statistics/log/",
)

COMPETITION_LOGGING_URL = os.getenv(
    "COMPETITION_LOGGING_URL",
    "https://sn2-api.inferencelabs.com/statistics/competition/log/",
)

EVAL_LOGGING_URL = os.getenv(
    "EVAL_LOGGING_URL",
    "https://sn2-api.inferencelabs.com/statistics/eval/log/",
)

session = requests.Session()
retries = Retry(total=3, backoff_factor=0.1)
session.mount("https://", HTTPAdapter(max_retries=retries))


def log_responses(
    metagraph: bt.metagraph,
    hotkey: bt.Keypair,
    uid: int,
    responses: list["MinerResponse"],
    overhead_time: float,
    block: int,
    scores: torch.Tensor,
) -> Optional[requests.Response]:
    """
    Log miner responses to the centralized logging server.
    """

    data = {
        "validator_key": hotkey.ss58_address,
        "validator_uid": uid,
        "overhead_duration": overhead_time,
        "block": block,
        "responses": [response.to_log_dict(metagraph) for response in responses],
        "scores": {k: float(v.item()) for k, v in enumerate(scores) if v.item() > 0},
    }

    input_bytes = json.dumps(data).encode("utf-8")
    # sign the inputs with your hotkey
    signature = hotkey.sign(input_bytes)
    # encode the inputs and signature as base64
    signature_str = base64.b64encode(signature).decode("utf-8")

    try:
        return session.post(
            LOGGING_URL,
            data=input_bytes,
            headers={
                "X-Request-Signature": signature_str,
                "Content-Type": "application/json",
            },
            timeout=5,
        )
    except requests.exceptions.RequestException as e:
        bt.logging.error(f"Failed to log responses: {e}")
        return None


def gc_log_competition_metrics(
    summary_data: dict, hotkey: bt.Keypair
) -> Optional[requests.Response]:
    """
    Logs a pre-formatted competition summary
    """
    try:

        if "validator_key" not in summary_data:
            bt.logging.warning(
                "Validator key not found in competition summary data, adding it."
            )
            summary_data["validator_key"] = hotkey.ss58_address

        input_bytes = json.dumps(summary_data).encode("utf-8")
        signature = hotkey.sign(input_bytes)
        signature_str = base64.b64encode(signature).decode("utf-8")

        bt.logging.trace(f"Logging competition summary: {summary_data}")

        return session.post(
            COMPETITION_LOGGING_URL,
            data=input_bytes,
            headers={
                "Content-Type": "application/json",
                "X-Request-Signature": signature_str,
            },
            timeout=10,
        )
    except requests.exceptions.RequestException as e:
        bt.logging.error(f"Failed to log competition summary: {e}")
        return None
    except Exception as e:
        bt.logging.error(
            f"Unexpected error logging competition summary: {e}", exc_info=True
        )
        return None


def gc_log_eval_metrics(
    model_id: str,
    model_name: str,
    netuid: int,
    weights_version: int,
    proof_system: str,
    circuit_type: str,
    proof_size: int,
    timeout: float,
    benchmark_weight: float,
    total_verifications: int,
    successful_verifications: int,
    min_response_time: float,
    max_response_time: float,
    avg_response_time: float,
    last_verification_time: int,
    last_block: int,
    verification_ratio: float,
    hotkey: bt.Keypair,
) -> Optional[requests.Response]:
    """
    Log circuit evaluation metrics to the centralized logging server.
    """
    try:
        data = {
            "validator_key": hotkey.ss58_address,
            "model_id": model_id,
            "model_name": model_name,
            "netuid": netuid,
            "weights_version": weights_version,
            "proof_system": proof_system,
            "circuit_type": circuit_type,
            "proof_size": proof_size,
            "timeout": timeout,
            "benchmark_weight": benchmark_weight,
            "total_verifications": total_verifications,
            "successful_verifications": successful_verifications,
            "min_response_time": min_response_time,
            "max_response_time": max_response_time,
            "avg_response_time": avg_response_time,
            "last_verification_time": last_verification_time,
            "last_block": last_block,
            "verification_ratio": verification_ratio,
        }

        input_bytes = json.dumps(data).encode("utf-8")
        signature = hotkey.sign(input_bytes)
        signature_str = base64.b64encode(signature).decode("utf-8")

        return session.post(
            EVAL_LOGGING_URL,
            data=input_bytes,
            headers={
                "Content-Type": "application/json",
                "X-Request-Signature": signature_str,
            },
            timeout=5,
        )
    except requests.exceptions.RequestException as e:
        bt.logging.error(f"Failed to log eval metrics: {e}")
        return None
