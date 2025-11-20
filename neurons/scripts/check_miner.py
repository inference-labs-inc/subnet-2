#! /usr/bin/env python3
"""
Usage instructions

In your command line, navigate into the neurons directory
cd neurons

Then, run the following command to check the miner

External IP and Port: Enter the target WAN IP and port of the miner server
Wallet and Hotkey: Enter your wallet name and hotkey name

scripts/check_miner.py --external_ip <external_ip> --port <port> --wallet <wallet> --hotkey <hotkey>

To debug an issue with the script or see more information, include --trace in the command line arguments.
"""
import argparse
import asyncio
import os
import sys

import bittensor as bt
import httpx

sys.path.append(os.path.join(os.path.dirname(__file__), ".."))

from deployment_layer.circuit_store import circuit_store
from protocol import QueryZkProof

from _validator.api.client import query_miner
from _validator.core.request import Request
from _validator.models.miner_response import MinerResponse
from _validator.models.request_type import RequestType

# Parse external IP and port from command line arguments
parser = argparse.ArgumentParser(description="Check the miner server", add_help=False)
required_named = parser.add_argument_group("required named arguments")
required_named.add_argument(
    "--external_ip", type=str, required=True, help="External IP of the miner"
)
parser.add_argument(
    "--port",
    type=int,
    help="Port on which the miner is running",
    default=8091,
)
parser.add_argument(
    "--wallet",
    type=str,
    help="Wallet name",
    default="default",
)
parser.add_argument(
    "--hotkey",
    type=str,
    help="Hotkey name",
    default="default",
)
parser.add_argument(
    "--trace",
    help="Enable trace logging",
    action="store_true",
)

args, unknown = parser.parse_known_args()


if args.trace:
    bt.logging.set_trace(True)


if __name__ == "__main__":
    bt.logging.info(
        f"Checking miner at {args.external_ip}:{args.port} using wallet {args.wallet} and hotkey {args.hotkey}"
    )

    # Create config that doesn't require coldkey password
    config = bt.config()
    config.wallet = bt.config()
    config.wallet.name = args.wallet
    config.wallet.hotkey = args.hotkey

    wallet = bt.wallet(config=config)
    circuit_store.load_circuits()

    request = Request(
        uid=0,
        ip=args.external_ip,
        port=args.port,
        hotkey=wallet.hotkey.ss58_address,
        coldkey="",  # Not needed for this script
        data=QueryZkProof(
            query_input={
                "list_items": [
                    0.6357621247078922,
                    0.6049274246433166,
                    0.550940686379023,
                    0.3682035751100801,
                    0.12160811389801046,
                ]
            },
            model_id="31df94d233053d9648c3c57362d9aa8aaa0f77761ac520af672103dbb387a6a5",
        ).model_dump(),
        url_path=QueryZkProof.name,
        request_type=RequestType.BENCHMARK,
        circuit=next(
            (c for c in circuit_store.circuits.values() if c.metadata.name == "LSTM"),
            None,
        ),
    )

    if request.circuit is None:
        bt.logging.error("No circuit with name 'LSTM' found. Aborting miner check.")
        sys.exit(1)

    async def run_query():
        async with httpx.AsyncClient() as client:
            return await query_miner(client, request, wallet)

    response = asyncio.run(run_query())

    if response is None:
        bt.logging.error(
            "No response from miner. Check your port is exposed correctly and the miner is running."
        )
        sys.exit(1)

    response = MinerResponse.from_raw_response(response)

    bt.logging.trace(f"Miner query response: {response}")
    if response and not response.error:
        bt.logging.trace(f"Status Message: {response.error}")
        bt.logging.success("Miner is running and ready to query.")
    else:
        bt.logging.error(
            "Failed to query miner. Check your port is exposed correctly and the miner is running."
        )
