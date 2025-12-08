import os
import shutil
from utils.system import get_temp_folder
import bittensor as bt


def cleanup_temp_dir(signum=None, frame=None, specific_dir=None):
    temp_folder = get_temp_folder()
    if not os.path.exists(temp_folder):
        return

    if specific_dir:
        dir_path = os.path.join(temp_folder, specific_dir)
        if os.path.exists(dir_path):
            try:
                if os.path.isfile(dir_path) or os.path.islink(dir_path):
                    os.unlink(dir_path)
                elif os.path.isdir(dir_path):
                    shutil.rmtree(dir_path)
            except Exception as e:
                bt.logging.error(f"Error cleaning up directory {dir_path}: {e}")
    else:
        bt.logging.debug("No specific directory provided for cleanup, skipping...")
