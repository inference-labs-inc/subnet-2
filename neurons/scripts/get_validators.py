#!/usr/bin/env python3
"""
Script to fetch all validators from a Bittensor subnet and output them as JSON.

Usage:
    python get_validators.py --netuid <NETUID> [--network <NETWORK>] [--output <FILE>]

Examples:
    # Mainnet subnet 2
    python get_validators.py --netuid 2 --network finney

    # Testnet subnet 118
    python get_validators.py --netuid 118 --network test

    # Output to file
    python get_validators.py --netuid 2 --network finney --output validators.json
"""

import argparse
import json
import sys

import bittensor as bt


def get_validators(
    netuid: int, network: str = "finney", subtensor: bt.Subtensor | None = None
) -> list[dict]:
    """
    Fetch all validators from a Bittensor subnet.

    Args:
        netuid: The subnet unique identifier
        network: The network to connect to ('finney' for mainnet, 'test' for testnet)
        subtensor: Optional pre-configured subtensor instance

    Returns:
        List of validator dictionaries with ss58, name, and enabled fields
    """
    if subtensor is None:
        print(f"Connecting to {network} network...", file=sys.stderr)
        subtensor = bt.Subtensor(network=network)

    print(f"Fetching neurons for netuid {netuid}...", file=sys.stderr)
    neurons = subtensor.neurons_lite(netuid)

    # Cache identity lookups by coldkey to avoid redundant queries
    identity_cache: dict[str, str | None] = {}

    validators = []
    validator_neurons = [n for n in neurons if n.validator_permit]
    total = len(validator_neurons)

    print(f"Found {total} validators, fetching identities...", file=sys.stderr)

    for i, neuron in enumerate(validator_neurons):
        # default validator info
        name = f"validator_{neuron.uid}"
        url = ""
        github = ""
        image = ""
        discord = ""
        description = ""
        additional = ""

        # Try to get identity name from coldkey
        coldkey = neuron.coldkey
        if coldkey not in identity_cache:
            identity_cache[coldkey] = subtensor.query_identity(neuron.coldkey)

        if identity_cache[coldkey] is not None:
            name = identity_cache[coldkey].name or name
            url = identity_cache[coldkey].url
            github = identity_cache[coldkey].github
            image = identity_cache[coldkey].image
            discord = identity_cache[coldkey].discord
            description = identity_cache[coldkey].description
            additional = identity_cache[coldkey].additional

        validators.append(
            {
                "ss58": neuron.hotkey,
                "name": name,
                "enabled": not neuron.is_null and neuron.active,
                "url": url,
                "github": github,
                "image": image,
                "discord": discord,
                "description": description,
                "additional": additional,
            }
        )

        # Progress indicator
        if (i + 1) % 10 == 0 or (i + 1) == total:
            print(f"  Processed {i + 1}/{total} validators", file=sys.stderr)

    print(f"Completed fetching {len(validators)} validators", file=sys.stderr)
    return validators


def main():
    parser = argparse.ArgumentParser(
        description="Fetch validators from a Bittensor subnet"
    )
    parser.add_argument(
        "--netuid",
        type=int,
        required=True,
        help="The subnet unique identifier (e.g., 2 for mainnet, 118 for testnet)",
    )
    parser.add_argument(
        "--network",
        type=str,
        default="finney",
        choices=["finney", "test", "local"],
        help="Network to connect to: 'finney' (mainnet), 'test' (testnet), or 'local'",
    )
    parser.add_argument(
        "--endpoint",
        type=str,
        default=None,
        help="Custom chain endpoint URL (e.g., ws://127.0.0.1:9944)",
    )
    parser.add_argument(
        "--output",
        "-o",
        type=str,
        default=None,
        help="Output file path (default: stdout)",
    )
    parser.add_argument(
        "--pretty",
        action="store_true",
        help="Pretty print JSON output",
    )

    args = parser.parse_args()

    # Create subtensor instance
    if args.endpoint:
        print(f"Using custom endpoint: {args.endpoint}", file=sys.stderr)
        subtensor = bt.Subtensor(chain_endpoint=args.endpoint)
    else:
        print(f"Connecting to {args.network} network...", file=sys.stderr)
        subtensor = bt.Subtensor(network=args.network)

    validators = get_validators(args.netuid, args.network, subtensor)

    # Format output
    if args.pretty:
        output = json.dumps(validators, indent=2)
    else:
        output = json.dumps(validators)

    # Write to file or stdout
    if args.output:
        with open(args.output, "w") as f:
            f.write(output)
        print(f"Output written to {args.output}", file=sys.stderr)
    else:
        print(output)


if __name__ == "__main__":
    main()
