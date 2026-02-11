import os
import traceback

# isort: off
import cli_parser  # <- this need to stay before bittensor import

import bittensor as bt

# isort: on

from _validator.validator_session import ValidatorSession
from constants import Roles
from utils import run_shared_preflight_checks, run_auto_update_check

if __name__ == "__main__":
    cli_parser.init_config(Roles.VALIDATOR)
    run_auto_update_check()
    run_shared_preflight_checks(Roles.VALIDATOR)

    if os.environ.get("ENABLE_MEMORY_PROFILER"):
        from utils.memory_profiler import start as start_memory_profiler

        start_memory_profiler()

    try:
        bt.logging.info("Creating validator session...")
        validator_session = ValidatorSession()
        bt.logging.debug("Running main loop...")
        validator_session.run()
    except Exception as e:
        bt.logging.error("Critical error while attempting to run validator: ", e)
        traceback.print_exc()
