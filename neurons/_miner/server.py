import hashlib
import threading

import bittensor as bt
import uvicorn
from bittensor_wallet import Wallet
from fastapi import APIRouter, Depends, FastAPI, Header, HTTPException, Request
from pydantic import BaseModel

from constants import (
    STEAK,
    VALIDATOR_STAKE_THRESHOLD,
)
from utils.signatures import Headers, verify_signature


class ZKRequestModel(BaseModel):
    query_input: dict


class MinerServer:

    def __init__(self, wallet: Wallet, config: bt.config, metagraph: bt.metagraph):
        self.wallet = wallet
        self.config = config
        self.metagraph = metagraph
        self.ip: str = self.config.axon.ip
        self.port: int = self.config.axon.port
        self.external_ip: str = self.config.axon.external_ip
        self.external_port: int = (
            self.config.axon.external_port
            if self.config.axon.external_port is not None
            else self.config.axon.port
        )
        self.server = None
        self.server_thread = None
        self.started = False

        self.nonces: dict[str, int] = {}

        # Instantiate FastAPI
        self.app = FastAPI()
        self.router = APIRouter()

    def _run_server(self):
        """Internal method to run the server in a thread."""
        self.server = uvicorn.Server(
            uvicorn.Config(self.app, host=self.external_ip, port=self.external_port)
        )
        self.server.run()

    def start(self):
        """Start the server in a background thread."""
        if self.started:
            bt.logging.warning("Server already started")
            return

        self.app.include_router(self.router)
        self.server_thread = threading.Thread(target=self._run_server, daemon=True)
        self.server_thread.start()
        self.started = True
        bt.logging.info(f"Server started on {self.ip}:{self.port} in background thread")

    def stop(self):
        """Stop the uvicorn server gracefully."""
        if self.server is not None:
            self.server.should_exit = True
            if self.server_thread and self.server_thread.is_alive():
                self.server_thread.join(timeout=5)
            bt.logging.info("Miner server stopped")
        self.started = False

    def register_route(self, path: str, endpoint):
        self.router.add_api_route(
            path,
            endpoint,
            dependencies=[Depends(self.blacklist), Depends(self.verify_request)],
            methods=["POST"],
        )

    async def verify_request(
        self,
        request: Request,
        validator_hotkey: str = Header(..., alias=Headers.VALIDATOR_HOTKEY),
        signature: str = Header(..., alias=Headers.SIGNATURE),
        miner_hotkey: str = Header(..., alias=Headers.MINER_HOTKEY),
        nonce: str = Header(..., alias=Headers.NONCE),
    ) -> None:
        body = await request.body()
        payload_hash = hashlib.sha256(body).hexdigest()
        message = f"{nonce}:{validator_hotkey}:{payload_hash}"

        if not verify_signature(
            message=message,
            signer_ss58_address=validator_hotkey,
            signature=signature,
        ):
            raise HTTPException(
                status_code=401,
                detail="Oi, invalid signature, you're not who you said you were!",
            )

        if miner_hotkey != self.wallet.hotkey.ss58_address:
            bt.logging.debug(
                f"Miner hotkey {miner_hotkey} does not match miner key {self.wallet.hotkey.ss58_address}"
            )
            raise HTTPException(
                status_code=401,
                detail="Oi, invalid miner hotkey - that's not me!",
            )

    async def blacklist(
        self,
        validator_hotkey: str = Header(..., alias=Headers.VALIDATOR_HOTKEY),
    ) -> None:
        """
        Filters requests if any of the following conditions are met:
        - Requesting hotkey is not registered
        - Requesting UID's stake is below 1k
        - Requesting UID does not have a validator permit

        Does not filter if the --disable-blacklist flag has been set.
        """
        try:
            if self.config.disable_blacklist:
                bt.logging.trace("Blacklist disabled, allowing request.")
                return

            if validator_hotkey not in self.metagraph.hotkeys:  # type: ignore
                raise HTTPException(status_code=403, detail="Hotkey is not registered")

            requesting_uid = self.metagraph.hotkeys.index(validator_hotkey)  # type: ignore
            stake = self.metagraph.S[requesting_uid].item()

            try:
                bt.logging.info(
                    f"Request by: {validator_hotkey} | UID: {requesting_uid} "  # type: ignore
                    f"| Stake: {stake} {STEAK}"
                )
            except UnicodeEncodeError:
                bt.logging.info(
                    f"Request by: {validator_hotkey} | UID: {requesting_uid} | Stake: {stake}"  # type: ignore
                )

            if stake < VALIDATOR_STAKE_THRESHOLD:
                raise HTTPException(status_code=403, detail="Stake below minimum")

            validator_permit = self.metagraph.validator_permit[requesting_uid].item()
            if not validator_permit:
                raise HTTPException(
                    status_code=403, detail="Requesting UID has no validator permit"
                )

            bt.logging.trace(f"Allowing request from UID: {requesting_uid}")

        except Exception as e:
            bt.logging.error(f"Error during blacklist {e}")
            raise
