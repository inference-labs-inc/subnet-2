import base64
import json
import os
import time
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

EVAL_LOGGING_URL = os.getenv(
    "EVAL_LOGGING_URL",
    "https://sn2-api.inferencelabs.com/statistics/eval/log/",
)

HEALTH_LOGGING_URL = os.getenv(
    "HEALTH_LOGGING_URL",
    "https://sn2-api.inferencelabs.com/statistics/health/log/",
)

session = requests.Session()
retries = Retry(total=3, backoff_factor=0.1)
session.mount("https://", HTTPAdapter(max_retries=retries))


def log_responses(
    metagraph: bt.Metagraph,
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


class HealthMetricsBuffer:
    FLUSH_INTERVAL = 60

    def __init__(self):
        self._samples: list[dict] = []
        self._last_flush = time.monotonic()

    def push(self, snapshot: dict) -> Optional[dict]:
        self._samples.append(snapshot)
        if time.monotonic() - self._last_flush < self.FLUSH_INTERVAL:
            return None
        return self.flush()

    def flush(self) -> Optional[dict]:
        if not self._samples:
            return None
        try:
            rss_values = [s["rss_mb"] for s in self._samples]
            count = len(self._samples)
            aggregated = {
                "sample_count": count,
                "avg_rss_mb": sum(rss_values) / count,
                "min_rss_mb": min(rss_values),
                "max_rss_mb": max(rss_values),
                "avg_tensor_cache_keys": sum(
                    s["tensor_cache_keys"] for s in self._samples
                )
                / count,
                "avg_timing_entries": sum(s["timing_entries"] for s in self._samples)
                / count,
                "avg_active_tasks": sum(s["active_tasks"] for s in self._samples)
                / count,
                "avg_current_concurrency": sum(
                    s["current_concurrency"] for s in self._samples
                )
                / count,
                "avg_queue_size": sum(s["queue_size"] for s in self._samples) / count,
            }
            return aggregated
        except (KeyError, TypeError) as e:
            bt.logging.warning(
                f"Health buffer flush failed, dropping {len(self._samples)} samples: {e}"
            )
            return None
        finally:
            self._samples.clear()
            self._last_flush = time.monotonic()


def gc_log_health(
    hotkey: bt.Keypair,
    validator_uid: int,
    payload: dict,
) -> Optional[requests.Response]:
    try:
        data = {
            "validator_key": hotkey.ss58_address,
            "validator_uid": validator_uid,
            **payload,
        }
        input_bytes = json.dumps(data).encode("utf-8")
        signature = hotkey.sign(input_bytes)
        signature_str = base64.b64encode(signature).decode("utf-8")

        return session.post(
            HEALTH_LOGGING_URL,
            data=input_bytes,
            headers={
                "Content-Type": "application/json",
                "X-Request-Signature": signature_str,
            },
            timeout=5,
        )
    except requests.exceptions.RequestException as e:
        bt.logging.error(f"Failed to log health metrics: {e}")
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
