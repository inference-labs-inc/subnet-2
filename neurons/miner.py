import os
import traceback

# isort: off
import cli_parser  # <- this need to stay before bittensor import

import bittensor as bt

# isort: on

from _miner.miner_session import MinerSession
from constants import Roles
from utils import run_shared_preflight_checks, run_auto_update_check

if __name__ == "__main__":
    cli_parser.init_config(Roles.MINER)
    run_auto_update_check()
    run_shared_preflight_checks(Roles.MINER)

    if os.getenv("SUBNET_2_DOCKER_BUILD"):
        bt.logging.info("Docker build mode: skipping circuit load and miner session")
        raise SystemExit(0)

    try:
        from deployment_layer.circuit_store import circuit_store

        circuit_store.load_circuits()

        bt.logging.info("Creating miner session...")
        miner_session = MinerSession()
        bt.logging.debug("Running main loop...")
        miner_session.run()
    except Exception:
        bt.logging.error(
            f"CRITICAL: Failed to run miner session\n{traceback.format_exc()}"
        )
