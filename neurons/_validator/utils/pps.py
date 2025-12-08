import time
import bittensor as bt
import requests
from substrateinterface import Keypair


class ProofPublishingService:
    def __init__(self, url: str):
        self.url = url

    def publish_proof(self, proof_json: dict, hotkey: Keypair):
        """
        Publishes a proof to the proof publishing service.

        Args:
            proof_json (dict): The proof data as a JSON object
            hotkey (Keypair): The hotkey used to sign the proof
        """
        try:
            timestamp = str(int(time.time()))
            message = timestamp.encode("utf-8")
            signature = hotkey.sign(message)

            response = requests.post(
                f"{self.url}/proof",
                json={"proof": proof_json},
                headers={
                    "x-timestamp": timestamp,
                    "x-origin-ss58": hotkey.ss58_address,
                    "x-signature": signature.hex(),
                    "Content-Type": "application/json",
                },
                timeout=60,
            )

            if response.status_code == 200:
                response_json = response.json()
                bt.logging.success(f"Proof of weights uploaded to {self.url}")
                bt.logging.info(f"Response: {response_json}")
                return response_json
            else:
                bt.logging.warning(
                    f"Failed to upload proof of weights to {self.url}. Status code: {response.status_code}"
                )
                return None
        except Exception as e:
            bt.logging.warning(f"Error uploading proof of weights: {e}")
            return None
