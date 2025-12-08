#! /usr/bin/env python3
"""
Usage instructions

In your command line, navigate into the neurons directory
cd neurons

Then, run the following command to check the axon of a miner

External IP and Port: Enter the target WAN IP and port of the miner
Wallet and Hotkey: Enter your wallet name and hotkey name

scripts/check_miner_axon.py --external_ip <external_ip> --port <port> --wallet <wallet> --hotkey <hotkey>

To debug an issue with the script or see more information, include --trace in the command line arguments.
"""
from constants import ONE_MINUTE

import argparse
import os
import sys
import bittensor as bt
import requests

sys.path.append(os.path.join(os.path.dirname(__file__), ".."))

# flake8: noqa
from protocol import QueryZkProof

# Parse external IP and port from command line arguments
parser = argparse.ArgumentParser(description="Check miner axon", add_help=False)
required_named = parser.add_argument_group("required named arguments")
required_named.add_argument(
    "--external_ip", type=str, required=True, help="External IP of the miner"
)
parser.add_argument(
    "--port",
    type=int,
    help="Port on which the miner's axon is running",
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

query_input = {"model_id": [0], "public_inputs": [1, 1, 1, 1, 1]}

if __name__ == "__main__":
    bt.logging.info(
        f"Checking miner axon at {args.external_ip}:{args.port} using wallet {args.wallet} and hotkey {args.hotkey}"
    )
    try:
        url = f"http://{args.external_ip}:{args.port}/QueryZkProof"
        bt.logging.trace(f"Attempting HTTP connection via URL: {url}")
        http_response = requests.get(url, timeout=30)
        bt.logging.trace(f"HTTP Response Body: {http_response.text}")
        bt.logging.success(
            "HTTP connection established. Your port is open and your axon is responding."
        )

    except Exception as e:
        bt.logging.exception(
            "Failed to establish HTTP connection. This could indicate that the axon is not running or your port is not exposed."
            "Please check your configuration.\n",
            e,
        )
        raise e

    wallet = bt.wallet(name=args.wallet, hotkey=args.hotkey)
    axon = bt.axon(wallet=wallet, external_ip=args.external_ip, external_port=args.port)
    bt.logging.trace(f"Attempting to query axon: {axon}")
    response = bt.dendrite(wallet=wallet).query(
        [axon],
        QueryZkProof(query_input=query_input),
        deserialize=False,
        timeout=ONE_MINUTE,
    )
    bt.logging.trace(f"Dendrite query response: {response}")
    if response[0] is not None and not response[0].dendrite.status_message.startswith(
        "Failed"
    ):
        bt.logging.trace(f"Status Message: {response[0].dendrite.status_message}")
        bt.logging.success("Axon is running and ready to query.")
    else:
        bt.logging.error(
            "Failed to query axon. Check your port is exposed correctly and the axon is running."
        )
