import asyncio

import bittensor as bt
import httpx
from bittensor.core.chain_data import AxonInfo

from _validator.api.client import query_miner
from _validator.config import ValidatorConfig
from _validator.core.request import Request
from _validator.models.request_type import RequestType
from protocol import QueryForCapacities


class CapacityManager:
    def __init__(self, config: ValidatorConfig, httpx_client: httpx.AsyncClient):
        self.config = config
        self.httpx_client = httpx_client

    async def sync_capacities(self, miners_info: list[AxonInfo]):
        bt.logging.info(f"Syncing capacities for {len(miners_info)} miners...")
        request_data = QueryForCapacities()

        requests = [
            Request(
                uid=0,  # UID is not used in this context
                ip=miner_info.ip,
                port=miner_info.port,
                hotkey=miner_info.hotkey,
                coldkey=miner_info.coldkey,
                data=request_data.model_dump(),
                url_path=request_data.name,
                request_type=RequestType.BENCHMARK,
            )
            for miner_info in miners_info
        ]
        return await asyncio.gather(
            *(
                query_miner(self.httpx_client, request, self.config.wallet)
                for request in requests
            )
        )
