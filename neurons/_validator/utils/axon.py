import traceback
import bittensor as bt
from aiohttp.client_exceptions import InvalidUrlClientError
from _validator.core.request import Request


async def query_single_axon(dendrite: bt.dendrite, request: Request) -> Request | None:
    """
    Query a single axon with a request. Per Circuit query.

    Args:
        dendrite (bt.dendrite): The dendrite to use for querying.
        request (Request): The request to send.

    Returns:
        Request | None: The request with results populated, or None if the request failed.
    """

    try:
        result = await dendrite.call(
            target_axon=request.axon,
            synapse=request.synapse,
            timeout=request.circuit.timeout,
            deserialize=False,
        )

        if not result:
            return None
        request.result = result
        request.response_time = (
            result.dendrite.process_time
            if result.dendrite.process_time is not None
            else request.circuit.timeout
        )

        request.deserialized = result.deserialize()
        return request

    except InvalidUrlClientError:
        bt.logging.warning(
            f"Ignoring UID as axon is not a valid URL: {request.uid}. {request.axon.ip}:{request.axon.port}"
        )
        return None

    except Exception as e:
        bt.logging.warning(f"Failed to query axon for UID: {request.uid}. Error: {e}")
        traceback.print_exc()
        return None
