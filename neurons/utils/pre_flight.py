import asyncio
import json
import os
import subprocess
import time
import traceback
from typing import Optional
from constants import FIVE_MINUTES, Roles, DSPERSE_SLICES_FILE_NAME

# trunk-ignore(pylint/E0611)
import bittensor as bt
import ezkl
import requests

import cli_parser
from constants import IGNORED_MODEL_HASHES

from functools import partial
from collections import OrderedDict

LOCAL_SNARKJS_INSTALL_DIR = os.path.join(os.path.expanduser("~"), ".snarkjs")
LOCAL_SNARKJS_PATH = os.path.join(
    LOCAL_SNARKJS_INSTALL_DIR, "node_modules", ".bin", "snarkjs"
)
LOCAL_EZKL_PATH = os.path.join(os.path.expanduser("~"), ".ezkl", "ezkl")
TOOLCHAIN = "nightly-2024-09-30"
JOLT_VERSION = "dd9e5c4bcf36ffeb75a576351807f8d86c33ec66"

MINER_EXTERNAL_FILES = [
    "circuit.zkey",
    "pk.key",
]
VALIDATOR_EXTERNAL_FILES = [
    "circuit.zkey",
]

SYNC_LOG_PREFIX = "  SYNC  | "


async def download_srs(logrows):
    await ezkl.get_srs(logrows=logrows, commitment=ezkl.PyCommitments.KZG)


def run_shared_preflight_checks(role: Optional[Roles] = None):
    """
    This function executes a series of checks to ensure the environment is properly
    set up for both validator and miner operations.
    Checks:
    - Model files are synced up
    - Node.js >= 20 is installed
    - SnarkJS is installed

    Raises:
        Exception: If any of the pre-flight checks fail.
    """

    preflight_checks = OrderedDict(
        {
            "Ensuring Node.js version": ensure_nodejs_version,
            "Checking SnarkJS installation": ensure_snarkjs_installed,
            "Checking EZKL installation": ensure_ezkl_installed,
            "Syncing model files": partial(sync_model_files, role=role),
        }
    )

    bt.logging.info(" PreFlight | Running pre-flight checks")

    # Skip sync_model_files during docker build
    if os.getenv("SUBNET_2_DOCKER_BUILD", False):
        bt.logging.info(" PreFlight | Skipping model file sync")
        _ = preflight_checks.pop("Syncing model files")

    for check_name, check_function in preflight_checks.items():
        bt.logging.info(f" PreFlight | {check_name}")
        try:
            check_function()
            bt.logging.success(f" PreFlight | {check_name} completed successfully")
        except Exception as e:
            bt.logging.error(f"Failed {check_name.lower()}.", e)
            bt.logging.debug(f" PreFlight | {check_name} error details: {str(e)}")
            traceback.print_exc()
            raise e

    bt.logging.info(" PreFlight | Pre-flight checks completed.")


def ensure_ezkl_installed():
    """
    Ensure EZKL is installed by first checking if it exists, and if not,
    running the official installation script. Also verifies the version matches.
    """
    python_ezkl_version = ezkl.__version__
    try:
        if os.path.exists(LOCAL_EZKL_PATH):
            # Check version matches
            result = subprocess.run(
                [LOCAL_EZKL_PATH, "--version"],
                capture_output=True,
                text=True,
                check=True,
            )
            if python_ezkl_version in result.stdout:
                bt.logging.info(
                    f"EZKL is already installed with correct version: {python_ezkl_version}"
                )
                return
            else:
                bt.logging.warning("EZKL version mismatch, reinstalling...")

        # trunk-ignore(bandit/B605)
        subprocess.run(
            f"curl -s https://raw.githubusercontent.com/zkonduit/ezkl/main/install_ezkl_cli.sh | bash -s -- v{python_ezkl_version}",  # noqa
            shell=True,
            check=True,
        )
        bt.logging.info("EZKL installed successfully")

    except subprocess.CalledProcessError as e:
        bt.logging.error(f"Failed to install/verify EZKL: {e}")
        raise RuntimeError(
            "EZKL installation failed. Please install it manually."
        ) from e


def ensure_snarkjs_installed():
    """
    Ensure snarkjs is installed and available for use in a local .snarkjs directory.
    """

    try:
        # trunk-ignore(bandit/B603)
        subprocess.run(
            [LOCAL_SNARKJS_PATH, "r1cs", "info", "--help"],
            check=True,
            capture_output=True,
            text=True,
        )
        bt.logging.info(
            "snarkjs is already installed and available in the local directory."
        )
    except (subprocess.CalledProcessError, FileNotFoundError):
        bt.logging.warning(
            "snarkjs not found in local directory. Attempting to install..."
        )
        try:
            # Create the local installation directory if it doesn't exist
            os.makedirs(LOCAL_SNARKJS_INSTALL_DIR, exist_ok=True)

            # Install snarkjs in the local directory
            # trunk-ignore(bandit/B603)
            # trunk-ignore(bandit/B607)
            subprocess.run(
                [
                    "npm",
                    "install",
                    "--prefix",
                    LOCAL_SNARKJS_INSTALL_DIR,
                    "snarkjs@0.7.4",
                ],
                check=True,
            )
            bt.logging.info(
                "snarkjs has been successfully installed in the local directory."
            )
        except subprocess.CalledProcessError as e:
            bt.logging.error(f"Failed to install snarkjs: {e}")
            raise RuntimeError(
                "snarkjs installation failed. Please install it manually."
            ) from e


def sync_model_files(role: Optional[Roles] = None):
    """
    Sync external model files
    """
    MODEL_DIR = os.path.join(os.path.dirname(__file__), "..", "deployment_layer")

    loop = asyncio.get_event_loop()
    # Download SRS files to ~/.ezkl/srs for logrows 1 to 25
    for logrows in range(1, 26):
        if os.path.exists(
            os.path.join(os.path.expanduser("~"), ".ezkl", "srs", f"kzg{logrows}.srs")
        ):
            bt.logging.info(
                f"{SYNC_LOG_PREFIX}SRS for logrows={logrows} already exists, skipping..."
            )
            continue

        try:
            loop.run_until_complete(download_srs(logrows))
            bt.logging.info(
                f"{SYNC_LOG_PREFIX}Successfully downloaded SRS for logrows={logrows}"
            )
        except Exception as e:
            bt.logging.error(
                f"{SYNC_LOG_PREFIX}Failed to download SRS for logrows={logrows}: {e}"
            )

    for model_hash in os.listdir(MODEL_DIR):
        if not model_hash.startswith("model_"):
            continue

        if model_hash.split("_")[1] in IGNORED_MODEL_HASHES:
            bt.logging.info(
                SYNC_LOG_PREFIX
                + f"Ignoring model {model_hash} as it is in the ignored list."
            )
            continue

        metadata_file = os.path.join(MODEL_DIR, model_hash, "metadata.json")
        if not os.path.isfile(metadata_file):
            bt.logging.error(
                SYNC_LOG_PREFIX
                + f"Metadata file not found at {metadata_file} for {model_hash}. Skipping sync for this model."
            )
            continue

        try:
            with open(metadata_file, "r", encoding="utf-8") as f:
                metadata = json.load(f)
        except json.JSONDecodeError:
            bt.logging.error(
                SYNC_LOG_PREFIX + f"Failed to parse JSON from {metadata_file}"
            )
            continue

        external_files = metadata.get("external_files", {})
        for key, url in external_files.items():
            if (role == Roles.VALIDATOR and key not in VALIDATOR_EXTERNAL_FILES) or (
                role == Roles.MINER and key not in MINER_EXTERNAL_FILES
            ):
                bt.logging.info(
                    SYNC_LOG_PREFIX
                    + f"Skipping {key} for {model_hash} as it is not required for the {role}."
                )
                continue
            file_path = os.path.join(
                cli_parser.config.full_path_models, model_hash, key
            )
            os.makedirs(os.path.dirname(file_path), exist_ok=True)
            if os.path.isfile(file_path):
                bt.logging.info(
                    SYNC_LOG_PREFIX
                    + f"File {key} for {model_hash} already downloaded, skipping..."
                )
                continue
            download_file(url, file_path)

        dsperse_file_url = metadata.get("dsperse_file", None)
        if dsperse_file_url:
            file_path = os.path.join(
                cli_parser.config.full_path_models, model_hash, DSPERSE_SLICES_FILE_NAME
            )
            # dsperse files are just zip archives,
            # but later on we extract them to a folder named after the file without extension
            # so in case that folder already exists, we skip downloading and extracting again
            extracted_path = os.path.splitext(file_path)[0]
            if os.path.isdir(extracted_path) or os.path.isfile(file_path):
                bt.logging.info(
                    SYNC_LOG_PREFIX
                    + f"Dsperse file {key} for {model_hash} already downloaded, skipping..."
                )
            else:
                download_file(dsperse_file_url, file_path)


def download_file(url, file_path):
    """
    Download a file from a URL to a specified file path.
    """
    bt.logging.info(SYNC_LOG_PREFIX + f"Downloading {url} to {file_path}...")
    try:
        with requests.get(url, timeout=FIVE_MINUTES * 2, stream=True) as response:
            response.raise_for_status()
            with open(file_path, "wb") as f:
                for chunk in response.iter_content(chunk_size=8192):
                    f.write(chunk)
    except requests.RequestException as e:
        bt.logging.error(
            SYNC_LOG_PREFIX + f"Failed to download {url} to {file_path}: {e}"
        )


def ensure_nodejs_version():
    """
    Ensure that Node.js version 20 is installed
    If not installed, provide instructions for manual installation.
    """
    NODE_LOG_PREFIX = "  NODE  | "

    try:
        node_version = subprocess.check_output(["node", "--version"]).decode().strip()
        npm_version = subprocess.check_output(["npm", "--version"]).decode().strip()

        if node_version.startswith("v20.") or (
            node_version.startswith("v") and float(node_version[1:].split(".")[0]) > 20
        ):
            bt.logging.info(
                NODE_LOG_PREFIX
                + f"Node.js version {node_version} and npm version {npm_version} are installed."
            )
            return
    except (subprocess.CalledProcessError, FileNotFoundError):
        bt.logging.error(
            f"{NODE_LOG_PREFIX}Node.js is not installed or is not the correct version."
        )
        bt.logging.error(
            NODE_LOG_PREFIX
            + "\033[91mPlease install Node.js >= 20 using the following command\n./setup.sh --no-install\033[0m"
        )
        time.sleep(10)
        raise RuntimeError(
            "Node.js >= 20 is required but not installed. Please install it manually and restart the process."
        )


def is_safe_path(base_path, path):
    return os.path.realpath(path).startswith(os.path.realpath(base_path))


def safe_extract(tar, path):
    for member in tar.getmembers():
        member_path = os.path.join(path, member.name)
        if not is_safe_path(path, member_path):
            continue
        tar.extract(member, path)
