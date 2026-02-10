import json
import logging
import shutil
import threading
from datetime import datetime, timezone
from pathlib import Path

import httpx
from google.cloud import storage
from google.oauth2 import service_account

from constants import GCS_PROOF_BUCKET, GCS_PROOF_CREDENTIALS

logger = logging.getLogger(__name__)


def _get_gcs_client() -> storage.Client:
    if GCS_PROOF_CREDENTIALS:
        credentials = service_account.Credentials.from_service_account_file(
            GCS_PROOF_CREDENTIALS
        )
        return storage.Client(credentials=credentials)
    return storage.Client()


def _parse_slice_num(slice_num: str) -> tuple[str, int | None]:
    if "_tile_" in slice_num:
        parts = slice_num.split("_tile_")
        return parts[0], int(parts[1])
    return slice_num, None


def upload_run_proofs(
    run_uid: str,
    circuit_id: str,
    circuit_name: str,
    slices: dict,
    validator_key: str,
    sign_fn,
    api_url: str,
    run_dir: Path,
) -> None:
    thread = threading.Thread(
        target=_upload_run_proofs_sync,
        args=(
            run_uid,
            circuit_id,
            circuit_name,
            slices,
            validator_key,
            sign_fn,
            api_url,
            run_dir,
        ),
        daemon=True,
    )
    thread.start()


def _upload_run_proofs_sync(
    run_uid: str,
    circuit_id: str,
    circuit_name: str,
    slices: dict,
    validator_key: str,
    sign_fn,
    api_url: str,
    run_dir: Path,
) -> None:
    try:
        _do_upload(
            run_uid, circuit_id, circuit_name, slices, validator_key, sign_fn, api_url
        )
    finally:
        if run_dir.exists():
            shutil.rmtree(run_dir)
            logger.debug(f"Cleaned up run directory {run_dir}")


def _do_upload(
    run_uid: str,
    circuit_id: str,
    circuit_name: str,
    slices: dict,
    validator_key: str,
    sign_fn,
    api_url: str,
) -> None:
    if not GCS_PROOF_BUCKET:
        logger.debug("GCS_PROOF_BUCKET not configured, skipping proof upload")
        return

    try:
        client = _get_gcs_client()
    except Exception as e:
        logger.warning(f"Failed to create GCS client: {e}")
        return

    bucket = client.bucket(GCS_PROOF_BUCKET)
    prefix = f"{run_uid}_{circuit_id}"
    proof_records = []

    for slice_num, slice_data in slices.items():
        if not slice_data.proof_file or not Path(slice_data.proof_file).exists():
            continue
        if not slice_data.success:
            continue

        proof_path = Path(slice_data.proof_file)
        parent_slice, tile_idx = _parse_slice_num(slice_num)

        if tile_idx is not None:
            gcs_key = f"proofs/{prefix}/{parent_slice}_tile_{tile_idx}.proof"
        else:
            gcs_key = f"proofs/{prefix}/{slice_num}.proof"

        try:
            blob = bucket.blob(gcs_key)
            blob.upload_from_filename(str(proof_path))
            size_bytes = proof_path.stat().st_size
            proof_system = (
                slice_data.proof_system.value
                if hasattr(slice_data.proof_system, "value")
                else str(slice_data.proof_system)
            )

            proof_records.append(
                {
                    "run_uid": run_uid,
                    "circuit_id": circuit_id,
                    "circuit_name": circuit_name,
                    "slice_num": slice_num,
                    "parent_slice": parent_slice,
                    "tile_idx": tile_idx,
                    "proof_system": proof_system,
                    "gcs_key": gcs_key,
                    "size_bytes": size_bytes,
                    "validator_key": validator_key,
                    "timestamp": datetime.now(timezone.utc).isoformat(),
                }
            )
            logger.debug(f"Uploaded proof {gcs_key} ({size_bytes} bytes)")
        except Exception as e:
            logger.warning(f"Failed to upload proof for slice {slice_num}: {e}")

    if not proof_records:
        return

    _submit_proof_metadata(proof_records, validator_key, sign_fn, api_url)


def _submit_proof_metadata(
    records: list[dict],
    validator_key: str,
    sign_fn,
    api_url: str,
) -> None:
    try:
        payload = {"validator_key": validator_key, "proofs": records}
        body = json.dumps(payload, sort_keys=True, separators=(",", ":"))
        signature = sign_fn(body)
        if not signature:
            logger.warning("Failed to sign proof metadata request")
            return

        with httpx.Client(timeout=30.0) as client:
            response = client.post(
                f"{api_url}/proofs/",
                content=body,
                headers={
                    "Content-Type": "application/json",
                    "X-Request-Signature": signature,
                },
            )
            if response.status_code == 200:
                logger.debug(f"Submitted {len(records)} proof metadata records")
            else:
                logger.warning(
                    f"Failed to submit proof metadata: {response.status_code} - {response.text}"
                )
    except Exception as e:
        logger.warning(f"Failed to submit proof metadata: {e}")
