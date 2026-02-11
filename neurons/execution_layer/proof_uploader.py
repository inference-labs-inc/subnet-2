import base64
import json
import logging
import os
from typing import Optional

import httpx

import cli_parser

logger = logging.getLogger(__name__)

SN2_API_URL = os.getenv("SN2_API_URL", "https://sn2-api.inferencelabs.com")


def _get_api_url() -> str:
    return getattr(cli_parser.config, "sn2_api_url", None) or SN2_API_URL


def _get_wallet():
    return cli_parser.config.wallet


def _sign_body(body: str) -> str:
    wallet = _get_wallet()
    signature = wallet.hotkey.sign(body.encode())
    return base64.b64encode(signature).decode()


def _authenticated_post(url: str, payload: dict, timeout: float = 60.0) -> dict:
    body = json.dumps(payload)
    signature = _sign_body(body)
    with httpx.Client(timeout=timeout) as client:
        response = client.post(
            url,
            content=body,
            headers={
                "Content-Type": "application/json",
                "X-Request-Signature": signature,
            },
        )
        response.raise_for_status()
        return response.json()


def request_upload_urls(
    run_uid: str,
    circuit_id: str,
    artifacts: list[dict],
) -> list[dict]:
    api_url = _get_api_url()
    hotkey = _get_wallet().hotkey.ss58_address
    payload = {
        "validator_key": hotkey,
        "run_uid": run_uid,
        "circuit_id": circuit_id,
        "artifacts": artifacts,
    }
    result = _authenticated_post(f"{api_url}/proofs/upload-urls", payload)
    return result.get("urls", [])


def upload_artifact_bytes(client: httpx.Client, upload_url: str, data: bytes) -> bool:
    response = client.put(
        upload_url,
        content=data,
        headers={"Content-Type": "application/octet-stream"},
    )
    return response.status_code in (200, 201)


def confirm_uploads(
    run_uid: str,
    circuit_id: str,
    circuit_name: str,
    artifacts: list[dict],
) -> dict:
    api_url = _get_api_url()
    hotkey = _get_wallet().hotkey.ss58_address
    payload = {
        "validator_key": hotkey,
        "run_uid": run_uid,
        "circuit_id": circuit_id,
        "circuit_name": circuit_name,
        "artifacts": artifacts,
    }
    return _authenticated_post(f"{api_url}/proofs/confirm", payload)


def upload_final_output(
    run_uid: str,
    circuit_id: str,
    output: dict,
) -> Optional[str]:
    api_url = _get_api_url()
    hotkey = _get_wallet().hotkey.ss58_address
    payload = {
        "validator_key": hotkey,
        "run_uid": run_uid,
        "circuit_id": circuit_id,
        "output": output,
    }
    result = _authenticated_post(f"{api_url}/proofs/{run_uid}/output", payload)
    return result.get("gcs_key")


def upload_run_proofs(
    run_uid: str,
    circuit_id: str,
    circuit_name: str,
    proof_artifacts: list[dict],
    final_output: Optional[dict] = None,
) -> bool:
    """
    Upload all proofs for a completed run via signed URLs.

    proof_artifacts: list of dicts with keys:
        - slice_num: str
        - proof_system: str
        - proof_data: bytes | str (hex for JSTPROVE, dict for EZKL)
        - parent_slice: Optional[str]
        - tile_idx: Optional[int]
    """
    try:
        hotkey = _get_wallet().hotkey.ss58_address
        if hotkey == "unknown":
            logger.warning(f"Skipping proof upload for run {run_uid}: invalid hotkey")
            return False
    except Exception:
        logger.warning(f"Skipping proof upload for run {run_uid}: wallet not available")
        return False

    if not proof_artifacts:
        logger.info(f"No proof artifacts to upload for run {run_uid}")
        return True

    try:
        artifact_specs = [
            {"slice_num": a["slice_num"], "artifact_type": "proof"}
            for a in proof_artifacts
        ]
    except (KeyError, TypeError) as e:
        logger.error(f"Malformed proof artifact for run {run_uid}: {e}")
        return False

    try:
        url_responses = request_upload_urls(run_uid, circuit_id, artifact_specs)
    except Exception as e:
        logger.error(f"Failed to request upload URLs for run {run_uid}: {e}")
        return False

    try:
        url_map = {(u["slice_num"], u["artifact_type"]): u for u in url_responses}
    except (KeyError, TypeError) as e:
        logger.error(f"Malformed upload URL response for run {run_uid}: {e}")
        return False

    confirm_artifacts = []
    ok = True
    with httpx.Client(timeout=120.0) as client:
        for artifact in proof_artifacts:
            try:
                key = (artifact["slice_num"], "proof")
                url_info = url_map.get(key)
                if not url_info:
                    logger.warning(f"No upload URL for {key}")
                    ok = False
                    continue

                proof_data = artifact["proof_data"]
                if isinstance(proof_data, str):
                    data_bytes = bytes.fromhex(proof_data)
                elif isinstance(proof_data, dict):
                    data_bytes = json.dumps(proof_data).encode()
                elif isinstance(proof_data, bytes):
                    data_bytes = proof_data
                else:
                    logger.warning(
                        f"Unknown proof data type for {key}: {type(proof_data)}"
                    )
                    ok = False
                    continue

                uploaded = upload_artifact_bytes(
                    client, url_info["upload_url"], data_bytes
                )
                if not uploaded:
                    logger.warning(f"Failed to upload artifact {key}")
                    ok = False
                    continue

                confirm_artifacts.append(
                    {
                        "slice_num": artifact["slice_num"],
                        "parent_slice": artifact.get("parent_slice"),
                        "tile_idx": artifact.get("tile_idx"),
                        "proof_system": artifact["proof_system"],
                        "gcs_key": url_info["gcs_key"],
                        "size_bytes": len(data_bytes),
                        "artifact_type": "proof",
                    }
                )
            except Exception as e:
                logger.error(
                    f"Error processing artifact {artifact.get('slice_num', '?')}: {e}"
                )
                ok = False
                continue

    if confirm_artifacts:
        try:
            confirm_uploads(run_uid, circuit_id, circuit_name, confirm_artifacts)
            logger.info(
                f"Confirmed {len(confirm_artifacts)} artifacts for run {run_uid}"
            )
        except Exception as e:
            logger.error(f"Failed to confirm uploads for run {run_uid}: {e}")
            ok = False

    if final_output:
        try:
            upload_final_output(run_uid, circuit_id, final_output)
            logger.info(f"Uploaded final output for run {run_uid}")
        except Exception as e:
            logger.error(f"Failed to upload final output for run {run_uid}: {e}")
            ok = False

    return ok
