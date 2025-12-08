"""
Entry point for running just preflight checks:
 - Check CLI args (in fact completely unnecessary here)
 - Model files are synced up
 - Node.js >= 20 is installed
 - SnarkJS is installed

This script is created to be called during the Docker image build process
to ensure that all dependencies are installed.
"""

# isort: off
import cli_parser  # <- this need to stay before bittensor import

import bittensor as bt

# isort: on

from utils import run_shared_preflight_checks

if __name__ == "__main__":
    cli_parser.init_config()
    # Run preflight checks, and that's it
    run_shared_preflight_checks()

    bt.logging.info("Preflight checks completed. Exiting...")
