from __future__ import annotations
import time
import hashlib
import boto3
import traceback
import threading
from typing import Optional, Dict
from substrateinterface import Keypair
from pathlib import Path
from botocore.config import Config
import bittensor as bt
from constants import ACTIVE_COMPETITION
from pydantic import BaseModel
import cli_parser


class CircuitCommitment(BaseModel):
    """
    Represents a circuit commitment with signed URLs for validator access.

    Attributes:
        vk_hash (str): SHA256 hash of model.compiled - this is committed on-chain
        file_urls (Dict[str, str]): Map of filenames to signed URLs for validator download
        expiry (int): Unix timestamp when URLs expire
        signature (str): Hotkey signature of commitment data
        last_modified (int): Unix timestamp of when circuit files were last modified
    """

    vk_hash: str
    file_urls: Dict[str, str]
    expiry: int
    signature: str
    last_modified: int


class CircuitManager:
    """
    Manages circuit file monitoring, cloud storage uploads, and chain commitments.

    This class ensures synchronization between:
    1. Local circuit files
    2. Cloud storage (R2/S3)
    3. On-chain commitments

    It periodically checks for changes in circuit files and automatically:
    - Uploads modified files to cloud storage
    - Updates on-chain commitments
    - Provides signed URLs to validators

    Security:
    - Ensures hash commitment matches VK before providing URLs
    - Maintains lockstep between chain state and file state
    - Prevents race conditions in updates
    """

    def __init__(
        self,
        wallet: Keypair,
        netuid: int,
        circuit_dir: str,
        storage_config: dict,
        check_interval: int = 60,
        existing_vk_hash: Optional[str] = None,
    ):
        """
        Initialize the CircuitManager.

        Args:
            wallet: Bittensor wallet for signing
            netuid: Network UID
            circuit_dir: Directory containing circuit files
            storage_config: Storage configuration dict containing:
                - provider: 'r2' or 's3'
                - bucket: bucket name
                - account_id: account ID (required for R2)
                - access_key: access key ID
                - secret_key: secret key
                - region: region (required for S3)
            check_interval: How often to check for changes (seconds)
        """
        self.wallet = wallet
        self.netuid = netuid
        self.circuit_dir = Path(circuit_dir)
        self.check_interval = check_interval
        self.storage_config = storage_config
        self.bucket = storage_config["bucket"]

        self.subtensor = bt.subtensor(config=cli_parser.config)
        bt.logging.debug(
            "Created dedicated subtensor instance for circuit manager using cli config"
        )

        if not storage_config or not storage_config["provider"]:
            raise ValueError(
                "Storage configuration is required to initialize CircuitManager."
            )

        if storage_config["provider"] == "r2":
            self.storage = boto3.client(
                "s3",
                endpoint_url=f"https://{storage_config['account_id']}.r2.cloudflarestorage.com",
                aws_access_key_id=storage_config["access_key"],
                aws_secret_access_key=storage_config["secret_key"],
                config=Config(
                    retries={"max_attempts": 3},
                    connect_timeout=5,
                    read_timeout=30,
                    region_name="auto",
                ),
            )
        else:
            self.storage = boto3.client(
                "s3",
                aws_access_key_id=storage_config["access_key"],
                aws_secret_access_key=storage_config["secret_key"],
                region_name=storage_config["region"],
                config=Config(
                    retries={"max_attempts": 3}, connect_timeout=5, read_timeout=30
                ),
            )

        self.current_vk_hash = existing_vk_hash
        self.last_upload_time: Optional[int] = None
        self._current_object_keys: Optional[Dict[str, str]] = None
        self._lock = threading.Lock()
        self._stop_event = threading.Event()

        if existing_vk_hash and self._calculate_vk_hash() == existing_vk_hash:
            try:
                required_files = ["model.compiled", "settings.json"]
                self._current_object_keys = {
                    fname: f"{existing_vk_hash}/{fname}" for fname in required_files
                }
                self.last_upload_time = int(time.time())
                bt.logging.info(
                    f"Initialized with existing VK hash: {existing_vk_hash[:8]}..."
                )
            except Exception as e:
                bt.logging.error(f"Failed to initialize with existing hash: {str(e)}")

        self._monitor_thread = threading.Thread(
            target=self._monitor_circuit_files, daemon=True
        )
        self._monitor_thread.start()

    def stop(self):
        """Stop the monitoring thread."""
        self._stop_event.set()
        self._monitor_thread.join()

    def _calculate_vk_hash(self) -> Optional[str]:
        """
        Calculate SHA256 hash of model.compiled.

        Returns:
            str: Hex digest of hash, or None if file not found
        """
        vk_path = self.circuit_dir / "model.compiled"
        if not vk_path.exists():
            return None

        with open(vk_path, "rb") as f:
            return hashlib.sha256(f.read()).hexdigest()

    def _upload_circuit_files(self) -> Dict[str, str]:
        """
        Upload all circuit files to storage (R2 or S3).

        Returns:
            Dict[str, str]: Map of filenames to object keys
        """
        required_files = [
            "model.compiled",
            "settings.json",
        ]

        uploaded = {}
        for fname in required_files:
            fpath = self.circuit_dir / fname
            if not fpath.exists():
                bt.logging.warning(f"Missing required file: {fname}")
                continue

            key = f"{self._calculate_vk_hash()}/{fname}"
            self.storage.upload_file(str(fpath), self.bucket, key)
            uploaded[fname] = key

        return uploaded

    def _get_signed_urls(self, object_keys: Dict[str, str]) -> Dict[str, str]:
        """
        Generate signed URLs for all uploaded files.

        Args:
            object_keys: Map of filenames to storage object keys

        Returns:
            Dict[str, str]: Map of filenames to signed URLs
        """
        urls = {}
        for fname, key in object_keys.items():
            url = self.storage.generate_presigned_url(
                "get_object",
                Params={"Bucket": self.bucket, "Key": key},
                ExpiresIn=3600,
            )
            urls[fname] = url
        return urls

    def _commit_to_chain(self, vk_hash: str, max_retries: int = 3) -> bool:
        """
        Commit circuit hash to chain with retries.

        Args:
            vk_hash: Hash to commit
            max_retries: Maximum number of retry attempts

        Returns:
            bool: True if commit succeeded, False otherwise
        """
        for attempt in range(max_retries):
            try:
                bt.logging.debug(
                    f"Committing hash {vk_hash[:8]}... (attempt {attempt + 1}/{max_retries})"
                )
                self.subtensor.commit(
                    wallet=self.wallet,
                    netuid=self.netuid,
                    data=vk_hash,
                )
                bt.logging.success(f"Successfully committed hash {vk_hash[:8]}...")
                return True
            except Exception as e:
                if attempt < max_retries - 1:
                    bt.logging.warning(f"Commit attempt {attempt + 1} failed: {str(e)}")
                    time.sleep(2**attempt)
                else:
                    bt.logging.error(
                        f"All commit attempts failed for hash {vk_hash[:8]}"
                    )
                    bt.logging.debug(f"Final error: {str(e)}")
                    return False
        return False

    def _monitor_circuit_files(self):
        """
        Continuously monitor circuit files for changes.

        This method:
        1. Checks VK hash periodically
        2. If changed, uploads all files
        3. Updates chain commitment
        """
        while not self._stop_event.is_set():
            try:
                with self._lock:
                    if not ACTIVE_COMPETITION:
                        time.sleep(self.check_interval)
                        continue

                    new_vk_hash = self._calculate_vk_hash()
                    if not new_vk_hash:
                        bt.logging.warning(
                            "No verification key found. This means you are not participating in the competition.\n"
                            "See https://sn2-accelerate.inferencelabs.com for details."
                        )
                        time.sleep(self.check_interval)
                        continue

                    if new_vk_hash != self.current_vk_hash:
                        bt.logging.info("Circuit files changed, uploading...")

                        object_keys = self._upload_circuit_files()
                        upload_time = int(time.time())

                        if not self._commit_to_chain(new_vk_hash):
                            bt.logging.error(
                                "Failed to commit to chain, will retry next cycle"
                            )
                            time.sleep(self.check_interval)
                            continue

                        self.current_vk_hash = new_vk_hash
                        self.last_upload_time = upload_time
                        self._current_object_keys = object_keys

                        bt.logging.success(
                            f"Upload complete. Updated circuit commitment: {new_vk_hash[:8]}..."
                        )

            except Exception as e:
                bt.logging.error(f"Error in circuit monitor: {str(e)}")
                bt.logging.error(traceback.format_exc())
            time.sleep(self.check_interval)

    def get_current_commitment(self) -> Optional[CircuitCommitment]:
        """
        Get current circuit commitment with signed URLs.

        This is called by the Competition synapse handler.

        Returns:
            CircuitCommitment: Current commitment info with signed URLs,
                             or None if not ready
        """
        with self._lock:

            if not (
                self.current_vk_hash
                and self.last_upload_time
                and self._current_object_keys
            ):
                return None

            urls = self._get_signed_urls(self._current_object_keys)
            expiry = int(time.time()) + 3600

            commitment = CircuitCommitment(
                vk_hash=self.current_vk_hash,
                file_urls=urls,
                expiry=expiry,
                last_modified=self.last_upload_time,
                signature=self.wallet.hotkey.sign(
                    f"{self.current_vk_hash}:{expiry}"
                ).hex(),
            )

            return commitment
