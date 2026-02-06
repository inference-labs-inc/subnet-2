import os
import sys
import shutil
import subprocess
import threading
from typing import Optional

from bittensor import logging
from constants import TEMP_FOLDER


def restart_app():
    logging.success("App restarting due to auto-update")
    python = sys.executable
    # trunk-ignore(bandit/B606)
    os.execl(python, python, *sys.argv)


def clean_temp_files():
    logging.info("Deleting temp folder...")
    folder_path = TEMP_FOLDER
    if os.path.exists(folder_path):
        logging.debug("Removing temp folder...")
        shutil.rmtree(folder_path)
    else:
        logging.info("Temp folder does not exist")


def get_temp_folder() -> str:
    if not os.path.exists(TEMP_FOLDER):
        os.makedirs(TEMP_FOLDER, exist_ok=True)
    return TEMP_FOLDER


def capture_environment() -> dict:
    """
    Capture environment state for dsperse metrics collection.
    Includes dependency versions, system resources, and binary availability.
    """
    try:
        import psutil
    except ImportError:
        psutil = None

    def get_jst_version() -> Optional[str]:
        try:
            from dsperse.src.backends.jstprove import JSTprove

            return JSTprove.get_version()
        except Exception:
            return None

    def get_ezkl_version() -> Optional[str]:
        try:
            import ezkl

            return ezkl.__version__
        except Exception:
            return None

    def check_mpi_installed() -> bool:
        try:
            subprocess.run(
                ["mpirun", "--version"],
                capture_output=True,
                check=True,
                timeout=2,
            )
            return True
        except (
            subprocess.CalledProcessError,
            FileNotFoundError,
            subprocess.TimeoutExpired,
        ):
            return False

    jst_version = get_jst_version()
    ezkl_version = get_ezkl_version()

    env = {
        "jst_installed": jst_version is not None
        or shutil.which("jstprove") is not None,
        "jst_version": jst_version,
        "mpi_installed": check_mpi_installed(),
        "ezkl_installed": ezkl_version is not None or shutil.which("ezkl") is not None,
        "ezkl_version": ezkl_version,
        "cpu_count": os.cpu_count(),
        "total_memory_gb": None,
        "available_memory_gb": None,
        "disk_free_gb": None,
    }

    if psutil:
        mem = psutil.virtual_memory()
        env["total_memory_gb"] = round(mem.total / (1024**3), 2)
        env["available_memory_gb"] = round(mem.available / (1024**3), 2)

    try:
        disk = shutil.disk_usage("/")
        env["disk_free_gb"] = round(disk.free / (1024**3), 2)
    except OSError as e:
        logging.debug(f"Failed to get disk usage: {e}")

    return env


class MemoryTracker:
    """
    Track peak memory usage during execution using psutil polling.
    """

    def __init__(self, poll_interval_sec: float = 0.1):
        self.poll_interval = poll_interval_sec
        self._peak_mb: float = 0.0
        self._running = False
        self._thread: Optional[threading.Thread] = None
        self._poll_error_logged = False

    def start(self):
        try:
            import psutil

            self._process = psutil.Process()
        except ImportError:
            return

        self._running = True
        self._peak_mb = 0.0

        def poll():
            import time

            while self._running:
                try:
                    mem_info = self._process.memory_info()
                    current_mb = mem_info.rss / (1024 * 1024)
                    if current_mb > self._peak_mb:
                        self._peak_mb = current_mb
                except (psutil.NoSuchProcess, psutil.AccessDenied):
                    self._running = False
                except Exception as e:
                    if not self._poll_error_logged:
                        logging.debug(f"MemoryTracker poll error: {e}")
                        self._poll_error_logged = True
                time.sleep(self.poll_interval)

        self._thread = threading.Thread(target=poll, daemon=True)
        self._thread.start()

    def stop(self) -> float:
        self._running = False
        if self._thread:
            self._thread.join(timeout=1.0)
        return self._peak_mb

    @property
    def peak_mb(self) -> float:
        return self._peak_mb
