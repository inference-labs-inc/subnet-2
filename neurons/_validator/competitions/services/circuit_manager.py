import os
import shutil
import json
import aiohttp
import bittensor as bt
from urllib.parse import urlparse
import traceback
from constants import ONE_HOUR
from protocol import Competition
import asyncio
import hashlib


class CircuitManager:
    def __init__(self, temp_dir: str, competition_id: int, dendrite: bt.dendrite):
        self.temp_dir = temp_dir
        self.competition_id = competition_id
        self.dendrite = dendrite
        os.makedirs(temp_dir, exist_ok=True)

    def cleanup_temp_files(self, circuit_dir: str):
        try:
            if os.path.exists(circuit_dir):
                shutil.rmtree(circuit_dir)
            temp_files = [
                os.path.join("/tmp/subnet-2", f)
                for f in ["temp_proof.json", "temp_witness.json"]
            ]
            for f in temp_files:
                if os.path.exists(f):
                    try:
                        os.remove(f)
                    except Exception as e:
                        bt.logging.warning(f"Failed to delete temp file {f}: {e}")
        except Exception as e:
            bt.logging.warning(f"Error cleaning up temp files: {e}")

    def _validate_url(self, url: str) -> bool:
        """Validate that URL is from R2 or S3"""
        try:
            parsed = urlparse(url)
            return any(
                domain in parsed.netloc
                for domain in [
                    "r2.cloudflarestorage.com",
                    "s3.amazonaws.com",
                    ".r2.dev",
                ]
            )
        except Exception:
            return False

    async def _download_large_file(
        self, session: aiohttp.ClientSession, url: str, file_path: str
    ) -> bool:
        try:
            async with session.get(url, timeout=ONE_HOUR) as response:
                response.raise_for_status()
                total_size = int(response.headers.get("content-length", 0))

                with open(file_path, "wb") as f:
                    downloaded = 0
                    chunk_size = 1024 * 1024
                    last_log_time = 0

                    async for chunk in response.content.iter_chunked(chunk_size):
                        if chunk:
                            f.write(chunk)
                            downloaded += len(chunk)

                            current_time = asyncio.get_event_loop().time()
                            if current_time - last_log_time >= 5:
                                progress = (
                                    (downloaded / total_size * 100)
                                    if total_size > 0
                                    else 0
                                )
                                bt.logging.debug(
                                    f"Download progress: {progress:.2f}% ({downloaded}/{total_size} bytes)"
                                )
                                last_log_time = current_time

                return True
        except Exception as e:
            bt.logging.error(f"Error downloading file: {str(e)}")
            if os.path.exists(file_path):
                os.remove(file_path)
            return False

    async def download_files(
        self, axon: bt.axon, expected_sha256: str, circuit_dir: str
    ) -> bool:
        try:
            bt.logging.info(
                f"Starting download of circuit files from miner {axon.ip}:{axon.port}"
            )

            if not self.dendrite:
                bt.logging.error("Dendrite not initialized")
                return False

            synapse = Competition(
                id=self.competition_id,
                hash=expected_sha256,
                file_name="commitment",
            )

            response = await self.dendrite.forward(
                axons=[axon],
                synapse=synapse,
                timeout=60,
                deserialize=True,
            )

            if not response or not response[0]:
                bt.logging.error("No response from axon")
                return False

            response_data = response[0]
            if isinstance(response_data, dict):
                response_synapse = Competition(**response_data)
            else:
                response_synapse = response_data

            if not response_synapse.commitment:
                bt.logging.warning("No commitment data in response")
                return False

            try:
                commitment = json.loads(response_synapse.commitment)
            except json.JSONDecodeError:
                bt.logging.error("Invalid commitment data")
                return False

            if "signed_urls" not in commitment:
                bt.logging.error("No signed URLs in commitment data")
                return False

            signed_urls = commitment["signed_urls"]
            required_files = ["settings.json", "model.compiled"]
            all_files_downloaded = True

            timeout = aiohttp.ClientTimeout(total=ONE_HOUR, connect=60)
            conn = aiohttp.TCPConnector(force_close=True, limit=1)

            async with aiohttp.ClientSession(
                connector=conn, timeout=timeout
            ) as session:
                for file_name in required_files:
                    if file_name not in signed_urls:
                        bt.logging.error(f"Missing signed URL for {file_name}")
                        all_files_downloaded = False
                        break

                    url = signed_urls[file_name]
                    if not self._validate_url(url):
                        bt.logging.error(f"Invalid URL for {file_name}: {url}")
                        all_files_downloaded = False
                        break

                    file_path = os.path.join(circuit_dir, file_name)
                    bt.logging.debug(f"Starting download of {file_name} from {url}")

                    if not await self._download_large_file(session, url, file_path):
                        all_files_downloaded = False
                        break

                    bt.logging.info(
                        f"Successfully downloaded {file_name} ({os.path.getsize(file_path)} bytes)"
                    )

            if all_files_downloaded:
                try:
                    model_path = os.path.join(circuit_dir, "model.compiled")
                    if not os.path.exists(model_path):
                        bt.logging.error("model.compiled not found after download")
                        return False

                    sha256 = hashlib.sha256()
                    with open(model_path, "rb") as fp:
                        for chunk in iter(lambda: fp.read(8 * 1024 * 1024), b""):
                            sha256.update(chunk)
                    computed = sha256.hexdigest()

                    if computed != expected_sha256:
                        bt.logging.error(
                            f"SHA256 mismatch for model.compiled: expected {expected_sha256[:8]}, got {computed[:8]}"
                        )
                        return False

                    bt.logging.success(
                        f"Successfully downloaded and verified files for {expected_sha256[:8]}"
                    )
                    return True
                except Exception as e:
                    bt.logging.error(f"Error verifying downloaded model hash: {e}")
                    return False
            else:
                bt.logging.error(
                    f"Failed to download all files for {expected_sha256[:8]}"
                )
                return False

        except Exception as e:
            bt.logging.error(f"Error downloading circuit files: {e}")
            bt.logging.error(f"Stack trace: {traceback.format_exc()}")
            return False
