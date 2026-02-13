import argparse
import os
import sys
from typing import Optional

from constants import (
    MAX_CONCURRENT_REQUESTS,
    ONCHAIN_PROOF_OF_WEIGHTS_ENABLED,
    PROOF_OF_WEIGHTS_INTERVAL,
    TEMP_FOLDER,
    Roles,
    COMPETITION_SYNC_INTERVAL,
    SN2_RELAY_URL,
)

SHOW_HELP = False

# Intercept --help/-h flags before importing bittensor since it overrides help behavior
# This allows showing our custom help message instead of bittensor's default one
if "--help" in sys.argv:
    SHOW_HELP = True
    sys.argv.remove("--help")
elif "-h" in sys.argv:
    SHOW_HELP = True
    sys.argv.remove("-h")

# flake8: noqa
import bittensor as bt

parser: Optional[argparse.ArgumentParser]
config: Optional[bt.Config]


DESCRIPTION = {
    Roles.MINER: "Subnet 2 Miner. Starts a Bittensor node that mines on the subnet.",
    Roles.VALIDATOR: "Subnet 2 Validator. Starts a Bittensor node that validates on the subnet.",
}


def init_config(role: Optional[str] = None):
    """
    Initialize the configuration for the node.
    Config is based on CLI arguments, some of which are common to both miner and validator,
    and some of which are specific to each.
    The configuration itself is stored in the global variable `config`. Kinda singleton pattern.
    """
    global parser
    global config

    if not os.path.exists(TEMP_FOLDER):
        os.makedirs(TEMP_FOLDER)

    parser = argparse.ArgumentParser(
        description=DESCRIPTION.get(role, ""),
        epilog="For more information, visit https://sn2-docs.inferencelabs.com",
        allow_abbrev=False,
    )

    # init common CLI arguments for both miner and validator:
    parser.add_argument("--netuid", type=int, default=1, help="The UID of the subnet.")
    parser.add_argument(
        "--no-auto-update",
        default=bool(os.getenv("SUBNET_2_NO_AUTO_UPDATE", False)),
        help="Whether this miner should NOT automatically update upon new release.",
        action="store_true",
    )
    parser.add_argument(
        "--disable-metric-logging",
        default=False,
        help="Whether to disable metric logging.",
        action="store_true",
    )
    parser.add_argument(
        "--dev",
        default=False,
        help="Whether to run in development mode for internal testing.",
        action="store_true",
    )
    parser.add_argument(
        "--localnet",
        action="store_true",
        default=False,
        help="Whether to run the miner in localnet mode.",
    )
    parser.add_argument(
        "--timeout",
        default=120,
        type=int,
        help="Timeout for requests in seconds (default: 120)",
    )
    parser.add_argument(
        "--external-model-dir",
        default=None,
        help="Custom location for storing models data (optional)",
    )
    parser.add_argument(
        "--dsperse-run-dir",
        default=None,
        help="Custom location for storing dsperse run data (optional)",
    )
    parser.add_argument(
        "--download-all-circuits",
        default=False,
        action="store_true",
        help="Download all circuits from API during startup (default: False)",
    )
    parser.add_argument(
        "--additional-circuits",
        type=lambda s: [x.strip() for x in s.split(",") if x.strip()],
        default=[],
        help="Comma-separated list of circuit IDs to include in addition to active circuits",
    )
    if role == Roles.VALIDATOR:
        # CLI arguments specific to the validator
        _validator_config()
    elif role == Roles.MINER:
        # CLI arguments specific to the miner
        _miner_config()
    else:
        bt.Subtensor.add_args(parser)
        bt.logging.add_args(parser)
        bt.Wallet.add_args(parser)
        config = bt.Config(parser, strict=True)

    if SHOW_HELP:
        # --help or -h flag was passed, show the help message and exit
        parser.print_help()
        sys.exit(0)

    if config.localnet:
        # quick localnet configuration set up for testing (common params for both miner and validator)
        if (
            config.subtensor.chain_endpoint
            == "wss://entrypoint-finney.opentensor.ai:443"
        ):
            # in case of default value, change to localnet
            config.subtensor.chain_endpoint = "ws://127.0.0.1:9944"
        if config.subtensor.network == "finney":
            config.subtensor.network = "local"
        config.eth_wallet = (
            config.eth_wallet if config.eth_wallet is not None else "0x002"
        )
        config.disable_metric_logging = True
        config.verbose = config.verbose if config.verbose is None else True

    config.full_path = os.path.expanduser("~/.bittensor/subnet-2")  # type: ignore
    config.full_path_score = os.path.join(config.full_path, "scores", "scores.pt")

    if config.external_model_dir:
        # user might have specified a custom location for storing models data
        # if not, we use the default location
        config.full_path_models = config.external_model_dir
    else:
        config.full_path_models = os.path.join(config.full_path, "models")

    if not config.dsperse_run_dir:
        config.dsperse_run_dir = os.path.join(config.full_path, "dsperse_runs")
    os.makedirs(config.dsperse_run_dir, exist_ok=True)

    os.makedirs(config.full_path, exist_ok=True)
    os.makedirs(config.full_path_models, exist_ok=True)
    os.makedirs(os.path.dirname(config.full_path_score), exist_ok=True)
    bt.logging(config=config, logging_dir=config.logging.logging_dir)
    bt.logging.enable_info()

    # Make sure we have access to the models directory
    if not os.access(config.full_path, os.W_OK):
        bt.logging.error(
            f"Cannot write to {config.full_path}. Please make sure you have the correct permissions."
        )


def _miner_config():
    """
    Add CLI arguments specific to the miner.
    """
    global parser
    global config

    parser.add_argument(
        "--disable-blacklist",
        default=None,
        action="store_true",
        help="Disables request filtering and allows all incoming requests.",
    )

    parser.add_argument(
        "--storage.provider",
        type=str,
        choices=["r2", "s3"],
        help="Storage provider (r2 or s3)",
        default=os.getenv("STORAGE_PROVIDER", "r2"),
    )

    parser.add_argument(
        "--storage.bucket",
        type=str,
        help="Storage bucket name for competition files",
        default=os.getenv("STORAGE_BUCKET") or os.getenv("R2_BUCKET"),
    )

    parser.add_argument(
        "--storage.account_id",
        type=str,
        help="Storage account ID (required for R2)",
        default=os.getenv("STORAGE_ACCOUNT_ID") or os.getenv("R2_ACCOUNT_ID"),
    )

    parser.add_argument(
        "--storage.access_key",
        type=str,
        help="Storage access key ID",
        default=os.getenv("STORAGE_ACCESS_KEY") or os.getenv("R2_ACCESS_KEY"),
    )

    parser.add_argument(
        "--storage.secret_key",
        type=str,
        help="Storage secret key",
        default=os.getenv("STORAGE_SECRET_KEY") or os.getenv("R2_SECRET_KEY"),
    )

    parser.add_argument(
        "--storage.region",
        type=str,
        help="Storage region (required for S3)",
        default=os.getenv("STORAGE_REGION", "us-east-1"),
    )

    bt.Subtensor.add_args(parser)
    bt.logging.add_args(parser)
    bt.Wallet.add_args(parser)
    bt.Axon.add_args(parser)

    config = bt.Config(parser, strict=True)

    if config.localnet:
        # quick localnet configuration set up for testing (specific params for miner)
        if config.wallet.name == "default":
            config.wallet.name = "miner"
        if not config.axon:
            config.axon = bt.Config()
            config.axon.ip = "127.0.0.1"
            config.axon.external_ip = "127.0.0.1"
        config.disable_blacklist = (
            config.disable_blacklist if config.disable_blacklist is not None else True
        )


def _validator_config():
    """
    Add CLI arguments specific to the validator.
    """
    global parser
    global config

    parser.add_argument(
        "--blocks_per_epoch",
        type=int,
        default=100,
        help="Number of blocks to wait before setting weights",
    )

    parser.add_argument(
        "--disable-statistic-logging",
        default=False,
        help="Whether to disable statistic logging.",
        action="store_true",
    )

    parser.add_argument(
        "--enable-pow",
        default=ONCHAIN_PROOF_OF_WEIGHTS_ENABLED,
        action="store_true",
        help="Whether proof of weights is enabled",
    )

    parser.add_argument(
        "--pow-target-interval",
        type=int,
        default=PROOF_OF_WEIGHTS_INTERVAL,
        help="The target interval for committing proof of weights to the chain",
    )

    parser.add_argument(
        "--ignore-external-requests",
        type=lambda x: x.lower() == "true",
        default=False,
        help="Whether to ignore external requests.",
    )

    parser.add_argument(
        "--competition-sync-interval",
        type=int,
        default=COMPETITION_SYNC_INTERVAL,
        help="The interval for syncing the competition in seconds. Defaults to 86400 (1 day).",
    )

    parser.add_argument(
        "--relay-url",
        type=str,
        default=SN2_RELAY_URL,
        help="WebSocket URL for the SN2 Relay service.",
    )

    parser.add_argument(
        "--prometheus-monitoring",
        action="store_true",
        default=False,
        help="Whether to enable prometheus monitoring.",
    )

    parser.add_argument(
        "--prometheus-port",
        type=int,
        default=9090,
        help="The port for the prometheus monitoring.",
    )

    parser.add_argument(
        "--max-concurrency",
        type=int,
        default=MAX_CONCURRENT_REQUESTS,
        help=f"Maximum concurrent in-flight requests (default: {MAX_CONCURRENT_REQUESTS}).",
    )

    parser.add_argument(
        "--api-miners-pct",
        type=int,
        default=20,
        help="Percentage of top-performing miners eligible for API requests (default: 20).",
    )

    parser.add_argument(
        "--disable-benchmark",
        action="store_true",
        default=False,
        help="Disable benchmark proof-of-weights requests.",
    )

    bt.Subtensor.add_args(parser)
    bt.logging.add_args(parser)
    bt.Wallet.add_args(parser)

    config = bt.Config(parser, strict=True)

    if config.localnet:
        # quick localnet configuration set up for testing (specific params for validator)
        if config.wallet.name == "default":
            config.wallet.name = "validator"
        config.disable_statistic_logging = True
        # Use local relay URL for localnet
        if config.relay_url == SN2_RELAY_URL:
            config.relay_url = "ws://localhost:8080"
