import hashlib
import json
import os
import platform
import subprocess
import sys
import time
from typing import Optional

import git
import requests

import cli_parser
from constants import REPO_URL, ONE_MINUTE
from bittensor import logging

from .system import restart_app

TARGET_BRANCH = "main"

GITHUB_API_RELEASES_URL = (
    REPO_URL.replace("github.com", "api.github.com/repos") + "/releases/latest"
)

PLATFORM_MAP = {
    ("Linux", "x86_64"): "linux-x86_64",
    ("Darwin", "arm64"): "macos-aarch64",
}

PYTHON_ONLY_FLAGS = frozenset({
    "--disable-blacklist",
    "--dev",
    "--localnet",
    "--disable-metric-logging",
    "--disable-statistic-logging",
    "--ignore-external-requests",
    "--enable-pow",
    "--prometheus-monitoring",
    "--download-all-circuits",
    "--verbose",
})

PYTHON_ONLY_VALUE_ARGS = frozenset({
    "--pow-target-interval",
    "--blocks_per_epoch",
    "--prometheus-port",
    "--max-benchmark-concurrent",
    "--additional-circuits",
    "--external-model-dir",
    "--dsperse-run-dir",
    "--timeout",
    "--storage.provider",
    "--storage.bucket",
    "--storage.account_id",
    "--storage.access_key",
    "--storage.secret_key",
    "--storage.region",
})


def get_version() -> Optional[str]:
    try:
        repo = git.Repo(search_parent_directories=True)
        tags = sorted(repo.tags, key=lambda t: t.commit.committed_datetime)
        return tags[-1].name if tags else None
    except Exception:
        return None


def run_auto_update_check():
    """
    Run auto-update check before preflight to avoid crash loops from broken preflight code.
    """
    if cli_parser.config.no_auto_update:
        return
    try:
        auto_updater = AutoUpdate()
        auto_updater.try_update()
    except Exception as e:
        logging.warning(f"Auto-update check failed: {e}")


class AutoUpdate:
    """
    Automatic update utility
    """

    def __init__(self):
        self.last_check_time = 0
        try:
            if not cli_parser.config.no_auto_update:
                self.repo = git.Repo(search_parent_directories=True)
        except Exception as e:
            logging.exception("Failed to initialize the repository", e)

    def get_local_latest_tag(self) -> Optional[git.Tag]:
        """
        Get the latest tag from the local git repository
        """
        try:
            tags = sorted(self.repo.tags, key=lambda t: t.commit.committed_datetime)
            current_tag: Optional[git.Tag] = tags[-1] if tags else None
            if current_tag:
                logging.info(f"Current tag: {current_tag.name}")
            return current_tag
        except Exception as e:
            logging.exception("Failed to get the current tag", e)
            return None

    def get_latest_release(self) -> Optional[dict]:
        try:
            headers = {"Accept": "application/vnd.github.v3+json"}
            response = requests.get(GITHUB_API_RELEASES_URL, headers=headers, timeout=10)
            response.raise_for_status()
            return response.json()
        except requests.RequestException as e:
            logging.exception("Failed to fetch the latest release from GitHub.", e)
            return None

    def get_latest_release_tag(self, release: Optional[dict] = None) -> Optional[str]:
        if release is None:
            release = self.get_latest_release()
        if release:
            return release["tag_name"]
        return None

    def attempt_packages_update(self):
        """
        Attempt to update the packages by installing the requirements from the requirements.txt file
        """
        logging.info("Attempting to update packages...")

        try:
            subprocess.check_call(
                [
                    "uv",
                    "sync",
                ],
                timeout=ONE_MINUTE,
            )
            logging.success("Successfully updated packages.")
        except Exception as e:
            logging.exception("Failed to update requirements", e)

    def update_to_latest_release(self, release: Optional[dict] = None) -> bool:
        """
        Update the repository to the latest release
        """
        try:

            if self.repo.is_dirty(untracked_files=False):
                logging.warning(
                    "Current changeset is dirty. Please commit changes, discard changes or update manually."
                )
                return False

            latest_release_tag_name = self.get_latest_release_tag(release)
            if not latest_release_tag_name:
                logging.error("Failed to fetch the latest release tag.")
                return False

            current_tag = self.get_local_latest_tag()

            if current_tag.name == latest_release_tag_name:
                if self.repo.head.commit.hexsha == current_tag.commit.hexsha:
                    logging.info("Your version is up to date.")
                    return False
                logging.info(
                    "Latest release is checked out, however your commit is different."
                )
            else:
                logging.trace(
                    f"Attempting to check out the latest release: {latest_release_tag_name}..."
                )
                self.repo.remote().fetch(quiet=True, tags=True, force=True)
                if latest_release_tag_name not in [tag.name for tag in self.repo.tags]:
                    logging.error(
                        f"Latest release tag {latest_release_tag_name} not found in the repository."
                    )
                    return False

            self.repo.git.checkout(latest_release_tag_name)
            logging.success(
                f"Successfully checked out the latest release: {latest_release_tag_name}"
            )
            return True

        except Exception as e:
            logging.exception(
                "Automatic update failed. Manually pull the latest changes and update.",
                e,
            )

        return False

    def _detect_role(self) -> str:
        script = os.path.basename(sys.argv[0])
        if "miner" in script:
            return "miner"
        return "validator"

    def _detect_platform_suffix(self) -> Optional[str]:
        key = (platform.system(), platform.machine())
        suffix = PLATFORM_MAP.get(key)
        if not suffix:
            logging.warning(f"No Rust binary available for platform {key}")
        return suffix

    def _find_asset_url(self, assets: list, name: str) -> Optional[str]:
        for asset in assets:
            if asset["name"] == name:
                return asset["browser_download_url"]
        return None

    def _download_file(self, url: str, dest: str) -> bool:
        try:
            with requests.get(url, stream=True, timeout=120) as resp:
                resp.raise_for_status()
                with open(dest, "wb") as f:
                    for chunk in resp.iter_content(chunk_size=65536):
                        f.write(chunk)
            return True
        except Exception as e:
            logging.warning(f"Failed to download {url}: {e}")
            try:
                if os.path.exists(dest):
                    os.remove(dest)
            except OSError:
                pass
            return False

    def _verify_checksum(self, filepath: str, expected_hash: str) -> bool:
        h = hashlib.sha256()
        with open(filepath, "rb") as f:
            while True:
                chunk = f.read(65536)
                if not chunk:
                    break
                h.update(chunk)
        actual = h.hexdigest()
        if actual != expected_hash:
            logging.warning(f"Checksum mismatch: expected {expected_hash}, got {actual}")
            return False
        return True

    def _parse_sha256sums(self, content: str) -> dict:
        result = {}
        for line in content.strip().splitlines():
            parts = line.split()
            if len(parts) == 2:
                result[os.path.basename(parts[1].lstrip("*"))] = parts[0]
        return result

    def _get_pm2_process_name(self) -> Optional[str]:
        if "PM2_HOME" not in os.environ:
            return None
        try:
            output = subprocess.check_output(
                ["pm2", "jlist"], timeout=10, stderr=subprocess.DEVNULL
            )
            processes = json.loads(output)
            pid = os.getpid()
            for proc in processes:
                if proc.get("pid") == pid:
                    return proc.get("name")
            logging.warning(f"PM2_HOME is set but current PID {pid} not found in pm2 process list")
            return None
        except Exception as e:
            logging.warning(f"PM2 process lookup failed: {e}")
            return None

    def _collect_rust_args(self) -> list:
        argv = sys.argv[1:]
        args = []
        skip_next = False
        for i, arg in enumerate(argv):
            if skip_next:
                skip_next = False
                continue

            bare_arg = arg.split("=")[0] if "=" in arg else arg

            if bare_arg in PYTHON_ONLY_FLAGS:
                logging.info(f"Dropping Python-only flag: {bare_arg}")
                continue

            if bare_arg in PYTHON_ONLY_VALUE_ARGS:
                logging.info(f"Dropping Python-only arg: {bare_arg}")
                if "=" not in arg and (i + 1) < len(argv) and not argv[i + 1].startswith("-"):
                    skip_next = True
                continue

            args.append(arg)
        return args

    def _ensure_binary(self, binary_path: str, binary_url: str, expected_hash: str, binary_name: str) -> bool:
        if os.path.exists(binary_path) and self._verify_checksum(binary_path, expected_hash):
            logging.info(f"Rust binary already installed and up to date at {binary_path}")
            return True

        logging.info(f"Downloading Rust binary: {binary_name}")
        tmp_path = binary_path + ".tmp"
        if not self._download_file(binary_url, tmp_path):
            return False

        if not self._verify_checksum(tmp_path, expected_hash):
            try:
                os.unlink(tmp_path)
            except OSError:
                pass
            return False

        os.chmod(tmp_path, 0o750)
        os.replace(tmp_path, binary_path)
        logging.info(f"Rust binary installed at {binary_path}")
        return True

    def try_rust_migration(self, release: Optional[dict] = None) -> bool:
        if release is None:
            release = self.get_latest_release()
        if not release:
            return False

        assets = release.get("assets", [])
        if not assets:
            return False

        platform_suffix = self._detect_platform_suffix()
        if not platform_suffix:
            return False

        role = self._detect_role()
        binary_name = f"sn2-{role}-{platform_suffix}"

        binary_url = self._find_asset_url(assets, binary_name)
        if not binary_url:
            return False

        sums_url = self._find_asset_url(assets, "SHA256SUMS")
        if not sums_url:
            logging.warning("Rust release found but SHA256SUMS asset missing, skipping migration")
            return False

        logging.info(f"Rust binary release detected: {binary_name} in {release['tag_name']}")

        repo_root = self.repo.working_dir
        binary_path = os.path.join(repo_root, f"sn2-{role}")
        sums_path = os.path.join(repo_root, ".sha256sums.tmp")

        if not self._download_file(sums_url, sums_path):
            return False

        try:
            with open(sums_path, "r") as f:
                checksums = self._parse_sha256sums(f.read())
        finally:
            os.unlink(sums_path)

        expected_hash = checksums.get(binary_name)
        if not expected_hash:
            logging.warning(f"No checksum found for {binary_name} in SHA256SUMS")
            return False

        if not self._ensure_binary(binary_path, binary_url, expected_hash, binary_name):
            return False

        rust_args = self._collect_rust_args()

        pm2_name = self._get_pm2_process_name()
        if pm2_name:
            logging.info(f"Reconfiguring PM2 process '{pm2_name}' to use Rust binary")
            try:
                subprocess.run(
                    ["pm2", "delete", pm2_name],
                    timeout=10, check=False, capture_output=True,
                )
                subprocess.run(
                    ["pm2", "start", binary_path, "--name", pm2_name, "--"] + rust_args,
                    timeout=10, check=True, capture_output=True,
                )
                subprocess.run(["pm2", "save"], timeout=10, check=False, capture_output=True)
                logging.info(f"PM2 process '{pm2_name}' reconfigured to Rust binary")
            except Exception as e:
                logging.warning(f"PM2 reconfiguration failed, re-registering Python process: {e}")
                try:
                    subprocess.run(
                        ["pm2", "start", sys.executable, "--name", pm2_name, "--"] + sys.argv,
                        timeout=10, check=True, capture_output=True,
                    )
                    subprocess.run(["pm2", "save"], timeout=10, check=False, capture_output=True)
                except Exception as re_err:
                    logging.warning(f"Failed to re-register Python process in PM2: {re_err}")
                return False
            sys.exit(0)
        else:
            logging.info(f"No PM2 detected, execing into Rust binary: {binary_path}")
            try:
                os.execl(binary_path, binary_path, *rust_args)
            except OSError as e:
                logging.warning(f"Failed to exec Rust binary: {e}")
                return False

    def try_update(self):
        if time.time() - self.last_check_time < 300:
            return

        self.last_check_time = time.time()

        release = self.get_latest_release()

        if self.try_rust_migration(release):
            return

        if not self.update_to_latest_release(release):
            return

        self.attempt_packages_update()

        logging.info("Restarting the application...")
        restart_app()
