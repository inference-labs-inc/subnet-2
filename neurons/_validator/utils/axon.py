import hashlib
import json
import time
import traceback

import bittensor as bt
import httpx
from aiohttp.client_exceptions import InvalidUrlClientError

# from substrateinterface.keypair import Keypair
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
            timeout=request.circuit.timeout,
            headers=headers,
            url=url,
        )
        end_time = time.perf_counter()

        # TODO: handle non-200 responses
        if response.status_code != 200:
            bt.logging.warning(
                f"Received non-200 response from miner {request.uid} at {url}: {response.status_code}"
            )
            return None

        result = response.json()

        if not result:
            return None
        request.response_time = end_time - start_time

        request.deserialized = result
        return request

    except InvalidUrlClientError:
        bt.logging.warning(
            f"Ignoring UID as there is not a valid URL: {request.uid}. {request.ip}:{request.port}"
        )
        return None

    except Exception as e:
        bt.logging.warning(f"Failed to query miner for UID: {request.uid}. Error: {e}")
        traceback.print_exc()
        return None


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
