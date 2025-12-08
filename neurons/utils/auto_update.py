import subprocess
import git
import time
import requests
from typing import Optional
from constants import REPO_URL, ONE_MINUTE
from .wandb_logger import safe_log

from bittensor import logging

from .system import restart_app
import cli_parser

TARGET_BRANCH = "main"


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

    def get_latest_release_tag(self):
        """
        Get the latest release tag from the GitHub repository
        """
        try:
            headers = {"Accept": "application/vnd.github.v3+json"}
            api_url = f"{REPO_URL.replace('github.com', 'api.github.com/repos')}/releases/latest"
            response = requests.get(api_url, headers=headers, timeout=10)
            response.raise_for_status()
            latest_release = response.json()
            return latest_release["tag_name"]
        except requests.RequestException as e:
            logging.exception("Failed to fetch the latest release from GitHub.", e)
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

    def update_to_latest_release(self) -> bool:
        """
        Update the repository to the latest release
        """
        try:

            if self.repo.is_dirty(untracked_files=False):
                logging.warning(
                    "Current changeset is dirty. Please commit changes, discard changes or update manually."
                )
                return False

            latest_release_tag_name = self.get_latest_release_tag()
            if not latest_release_tag_name:
                logging.error("Failed to fetch the latest release tag.")
                return False

            current_tag = self.get_local_latest_tag()

            safe_log(
                {
                    "local_version": current_tag.name,
                    "remote_version": latest_release_tag_name,
                }
            )

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

    def try_update(self):
        """
        Automatic update entrypoint method
        """

        if time.time() - self.last_check_time < 300:
            return

        self.last_check_time = time.time()

        if not self.update_to_latest_release():
            return

        self.attempt_packages_update()

        logging.info("Restarting the application...")
        restart_app()
