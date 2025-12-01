"""
Safe methods for WandB logging
"""

import bittensor as bt
import psutil
import torch
import wandb
import os
import threading
from queue import Queue
from typing import Dict, Any

ENTITY_NAME = "inferencelabs"
PROJECT_NAME = "subnet-2"
WANDB_ENABLED = False
_log_queue = Queue()
_log_thread = None


def _log_worker():
    while True:
        try:
            data = _log_queue.get()
            if data is None:
                break
            wandb.log(data)
        except Exception as e:
            bt.logging.debug(f"Failed to log to WandB in worker thread: {e}")
        finally:
            _log_queue.task_done()


def _ensure_log_thread():
    global _log_thread
    if _log_thread is None or not _log_thread.is_alive():
        _log_thread = threading.Thread(target=_log_worker, daemon=True)
        _log_thread.start()


def safe_login(api_key):
    """
    Attempts to log into WandB using a provided API key
    """
    try:
        bt.logging.debug("Attempting to log into WandB using provided API Key")
        wandb.login(key=api_key)
    except Exception as e:
        bt.logging.error(e)
        bt.logging.error("Failed to login to WandB. Your run will not be logged.")


def safe_init(name=None, wallet=None, metagraph=None, config=None):
    """
    Attempts to initialize WandB, and logs if unsuccessful
    """
    global WANDB_ENABLED
    if config and config.disable_wandb:
        bt.logging.warning("WandB logging disabled.")
        WANDB_ENABLED = False
        return
    try:
        bt.logging.debug("Attempting to initialize WandB")
        config_dict = {}
        try:
            config_dict["coldkey"] = getattr(
                getattr(wallet, "coldkeypub", None), "ss58_address", None
            )
        except Exception as e:
            bt.logging.debug(f"Failed to get ss58 for coldkeypub: {e}")
            config_dict["coldkey"] = None

        if wallet and metagraph and config:
            config_dict.update(
                {
                    "netuid": config.netuid,
                    "hotkey": wallet.hotkey.ss58_address,
                    "uid": metagraph.hotkeys.index(wallet.hotkey.ss58_address),
                    "cpu_physical": psutil.cpu_count(logical=False),
                    "cpu_logical": psutil.cpu_count(logical=True),
                    "cpu_freq": psutil.cpu_freq().max,
                    "memory": psutil.virtual_memory().total,
                }
            )

            # Log GPU specs if available
            if torch.cuda.is_available():
                config_dict.update(
                    {
                        "gpu_name": torch.cuda.get_device_name(0),
                        "gpu_memory": torch.cuda.get_device_properties(0).total_memory,
                    }
                )
        os.environ["WANDB_CONSOLE"] = "off"
        wandb.init(
            entity=ENTITY_NAME,
            project=PROJECT_NAME
            + ("-testnet" if config.subtensor.network == "test" else ""),
            name=name,
            config=config_dict,
            reinit=True,
        )
        WANDB_ENABLED = True
        _ensure_log_thread()
    except Exception as e:
        bt.logging.error(e)
        bt.logging.error("Failed to initialize WandB. Your run will not be logged.")
        WANDB_ENABLED = False


def safe_log(data: Dict[str, Any]):
    """
    Safely log data to WandB
    - Ignores request to log if WandB isn't configured
    - Logs to WandB if it is configured
    """

    if not WANDB_ENABLED:
        bt.logging.debug("Skipping log due to WandB logging disabled.")
        return

    try:
        bt.logging.debug("Attempting to log data to WandB")
        _log_queue.put(data)
    except Exception as e:
        bt.logging.debug("Failed to queue WandB log.")
        bt.logging.debug(e)
