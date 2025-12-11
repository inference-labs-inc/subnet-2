import hashlib
import json
import time

import bittensor as bt
import httpx

from _validator.models.miner_response import MinerResponse
from _validator.core.request import Request
from utils.signatures import Headers


async def query_miner(
    httpx_client: httpx.AsyncClient,
    request: Request,
    wallet: bt.wallet,
) -> MinerResponse:
    # Use httpx.URL for safer URL construction
    url = httpx.URL(
        scheme="http",
        host=request.ip,
        port=request.port,
        path=f"/{request.url_path.lstrip('/')}",
    )
    content = json.dumps(request.data)

    headers = get_headers(request, content, wallet)

    start_time = time.perf_counter()
    response = await httpx_client.post(
        url=url,
        content=content,
        timeout=request.circuit.timeout if request.circuit else None,
        headers=headers,
    )
    response.raise_for_status()
    end_time = time.perf_counter()

    request.response_time = end_time - start_time

    return MinerResponse.from_raw_response(request, response.json())


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
