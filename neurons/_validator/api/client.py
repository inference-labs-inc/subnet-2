import hashlib
import json
import time
import traceback

import bittensor as bt
import httpx

from _validator.core.request import Request
from utils.signatures import Headers


async def query_miner(
    httpx_client: httpx.AsyncClient,
    request: Request,
    wallet: bt.wallet,
) -> Request | None:
    try:
        url = f"http://{request.ip}:{request.port}/{request.url_path}"
        content = json.dumps(request.data)

        headers = get_headers(request, content, wallet)

        start_time = time.perf_counter()
        response = await httpx_client.post(
            content=content,
            timeout=request.circuit.timeout if request.circuit else None,
            headers=headers,
            url=url,
        )
        response.raise_for_status()
        end_time = time.perf_counter()

        result = response.json()
        request.response_time = end_time - start_time
        request.deserialized = result
        return request

    except httpx.InvalidURL:
        bt.logging.warning(
            f"Ignoring UID as there is not a valid URL: {request.uid}. {request.ip}:{request.port}"
        )
        return None

    except httpx.HTTPError as e:
        bt.logging.warning(f"Failed to query miner for UID: {request.uid}. Error: {e}")
        traceback.print_exc()
        raise e


def get_headers(request: Request, content: str, wallet: bt.wallet) -> dict:
    """
    Get headers for querying a miner.
    """

    validator_hotkey = wallet.hotkey.ss58_address
    miner_hotkey = request.hotkey
    nonce = str(time.time_ns())
    body_hash = hashlib.sha256(content.encode()).hexdigest()
    message = f"{nonce}:{validator_hotkey}:{body_hash}"
    signature = f"0x{wallet.hotkey.sign(message).hex()}"

    return {
        Headers.NONCE: nonce,
        Headers.SIGNATURE: signature,
        Headers.VALIDATOR_HOTKEY: validator_hotkey,
        Headers.MINER_HOTKEY: miner_hotkey,
        "Content-Type": "application/json",
    }
