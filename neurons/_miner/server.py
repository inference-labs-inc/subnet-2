import hashlib

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
        self.app = FastAPI()
        self.router = APIRouter()
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
        self.full_address = str(self.config.axon.ip) + ":" + str(self.config.axon.port)
        self.started = False

        self.nonces: dict[str, int] = {}

        # Instantiate FastAPI
        self.app = FastAPI()
        self.fast_config = uvicorn.Config(
            self.app,
            host="0.0.0.0",
            loop="none",
            port=self.external_port,
        )

    def start(self):
        self.app.include_router(self.router)
        uvicorn.run(self.app, host=self.ip, port=self.port)

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
                return False, "Allowed"

            if validator_hotkey not in self.metagraph.hotkeys:  # type: ignore
                return True, "Hotkey is not registered"

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
                return True, "Stake below minimum"

            validator_permit = self.metagraph.validator_permit[requesting_uid].item()
            if not validator_permit:
                return True, "Requesting UID has no validator permit"

            bt.logging.trace(f"Allowing request from UID: {requesting_uid}")
            return False, "Allowed"

        except Exception as e:
            bt.logging.error(f"Error during blacklist {e}")
            return True, "An error occurred while filtering the request"
