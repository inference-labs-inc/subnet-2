from __future__ import annotations
import os
import traceback
from fastapi import (
    FastAPI,
    WebSocket,
    WebSocketDisconnect,
    WebSocketException,
    Request,
    Response,
)
from proof_of_portfolio import verify
from fastapi.responses import JSONResponse

from jsonrpcserver import (
    async_dispatch,
    Success,
    Error,
    InvalidParams,
)

from fastapi.routing import APIRoute, APIWebSocketRoute
import bittensor as bt
from _validator.models.poc_rpc_request import ProofOfComputationRPCRequest
from _validator.models.pow_rpc_request import ProofOfWeightsRPCRequest
from _validator.models.base_rpc_request import QueuedRequestDataModel
import hashlib
from constants import (
    MAX_SIGNATURE_LIFESPAN,
    MAINNET_TESTNET_UIDS,
    EXTERNAL_REQUEST_QUEUE_TIME_SECONDS,
)
from _validator.config import ValidatorConfig
import base64
import substrateinterface
import time
from _validator.api.cache import ValidatorKeysCache
import threading
import uvicorn
from _validator.api.certificate_manager import CertificateManager
from _validator.api.websocket_manager import WebSocketManager
import asyncio
from OpenSSL import crypto
from deployment_layer.circuit_store import circuit_store
from _validator.utils.pps import ProofPublishingService
from constants import PPS_URL, TESTNET_PPS_URL

app = FastAPI()

recent_requests: dict[str, int] = {}


@app.middleware("http")
async def rate_limiter(request: Request, call_next):
    if request.url.path == "/rpc":
        return await call_next(request)

    ip = request.client.host
    if _should_rate_limit(ip):
        return Response(status_code=429)
    return await call_next(request)


def _should_rate_limit(ip: str):
    if ip in recent_requests.keys():
        return max(0, time.time() - recent_requests[ip]) < 1
    recent_requests[ip] = time.time()
    return False


class ValidatorAPI:
    def __init__(self, config: ValidatorConfig):
        self.config = config
        # a Queue of requests to be sent to miners
        # consists of "real world requests" ProofOfWeightsRPCRequest and ProofOfComputationRPCRequest
        # and a Request with one slice of a DSperse model (DSlice)
        self.stacked_requests_queue: list[QueuedRequestDataModel] = []
        self.ws_manager = WebSocketManager()
        self.recent_requests: dict[str, int] = {}
        self.validator_keys_cache = ValidatorKeysCache(config)
        self.server_thread: threading.Thread | None = None
        self.pending_requests: dict[str, asyncio.Event] = {}
        self.request_results: dict[str, dict[str, any]] = {}
        self.is_testnet = config.bt_config.subtensor.network == "test"
        self._setup_api()

    def _setup_api(self) -> None:
        if not self.config.api.enabled:
            bt.logging.info("API Disabled: --ignore-external-requests flag present")
            return

        bt.logging.debug("Starting WebSocket API server...")

        for route in self._get_routes():
            app.routes.append(route)

        if self.config.api.certificate_path:
            cert_manager = CertificateManager(self.config.api.certificate_path)
            cert_manager.ensure_valid_certificate(
                bt.axon(self.config.wallet).external_ip
            )
            self.commit_cert_hash()

        self.start_server()
        bt.logging.success("Ready to serve external requests")

    async def handle_ws(self, websocket: WebSocket):
        if (
            self.config.api.verify_external_signatures
            and not await self.validate_connection(websocket.headers)
        ):
            raise WebSocketException(code=3000, reason="Connection validation failed")

        try:
            await self.ws_manager.connect(websocket)
            async for data in websocket.iter_text():
                response = await async_dispatch(
                    data,
                    context=websocket,
                    methods={
                        "subnet-2.proof_of_weights": self.handle_proof_of_weights,
                        "subnet-2.proof_of_computation": self.handle_proof_of_computation,
                    },
                )
                await websocket.send_text(str(response))
        except WebSocketDisconnect:
            bt.logging.debug("Client disconnected normally")
        except Exception as e:
            bt.logging.error(f"WebSocket error: {str(e)}")
        finally:
            await self.ws_manager.disconnect(websocket)

    def get_circuits(self, request: Request) -> None:

        try:
            return JSONResponse(circuit_store.list_circuit_metadata())
        except Exception:
            bt.logging.error("Failed to fetch circuit metadata from circuit store.")
            traceback.print_exc()
        return Response(status_code=500)

    async def submit_proof(self, request: Request) -> Response:
        """
        Handles proof submission from authorized hotkeys.
        Verifies the proof and uploads to PPS if successful.
        """
        try:
            if not await self.validate_connection(request.headers):
                bt.logging.warning("Unauthorized proof submission attempt")
                return JSONResponse({"error": "Unauthorized"}, status_code=401)

            try:
                body = await request.json()
            except Exception:
                return JSONResponse({"error": "Invalid JSON body"}, status_code=400)

            proof_data = body.get("proof")
            public_signals = body.get("public_signals")

            if not proof_data:
                return JSONResponse({"error": "Missing proof data"}, status_code=400)
            try:
                verification_result = verify(proof_data, public_signals)

                if not verification_result:
                    return JSONResponse(
                        {"error": "Proof verification failed"}, status_code=400
                    )
            except Exception as e:
                bt.logging.error(f"Uploaded proof failed to verify {e}")
                return JSONResponse(
                    {"error": "Proof verification failed"}, status_code=400
                )

            try:
                pps_url = self._upload_to_pps(
                    proof_data, public_signals, request.headers
                )

                if pps_url:
                    return JSONResponse(
                        {
                            "verified": True,
                            "url": pps_url,
                        }
                    )
                else:
                    return JSONResponse(
                        {"verified": True, "error": "Failed to upload proof to PPS"},
                        status_code=200,
                    )

            except Exception as e:
                bt.logging.error(f"Error processing proof: {str(e)}")
                traceback.print_exc()
                return JSONResponse({"error": "Internal server error"}, status_code=500)

        except Exception as e:
            bt.logging.error(f"Error handling proof submission: {str(e)}")
            traceback.print_exc()
            return JSONResponse({"error": "Internal server error"}, status_code=500)

    def _upload_to_pps(self, proof_data, public_signals, headers) -> str | None:
        """
        Upload verified proof to PPS and return URL.
        """
        try:

            pps_url = TESTNET_PPS_URL if self.is_testnet else PPS_URL

            hotkey_ss58 = headers.get("x-origin-ss58")
            if not hotkey_ss58:
                bt.logging.error("No hotkey found in headers")
                return None

            pps = ProofPublishingService(pps_url)

            proof_json = {
                "proof": proof_data,
                "public_signals": public_signals,
                "submitter_hotkey": hotkey_ss58,
                "validator_hotkey": self.config.wallet.hotkey.ss58_address,
            }

            result = pps.publish_proof(proof_json, self.config.wallet.hotkey)

            if result and "gatewayUrl" in result:
                return result["gatewayUrl"]
            elif result:
                return f"{pps_url}/proof/{result.get('id', 'unknown')}"

            return None

        except Exception as e:
            bt.logging.error(f"PPS upload error: {str(e)}")
            traceback.print_exc()
            return None

    def _get_routes(self) -> list[APIWebSocketRoute | APIRoute]:
        rpc_endpoint = APIWebSocketRoute("/rpc", self.handle_ws)
        get_circuits_endpoint = APIRoute("/circuits", self.get_circuits)
        submit_proof_endpoint = APIRoute(
            "/verify-and-upload", self.submit_proof, methods=["POST"]
        )
        return [rpc_endpoint, get_circuits_endpoint, submit_proof_endpoint]

    async def handle_proof_of_weights(
        self, websocket: WebSocket, **params: dict[str, object]
    ) -> dict[str, object]:
        if not websocket.headers.get("x-netuid"):
            return InvalidParams(
                "Missing x-netuid header (required for proof of weights requests)"
            )

        evaluation_data = params.get("evaluation_data")
        weights_version = params.get("weights_version")

        if not evaluation_data:
            return InvalidParams("Missing evaluation data")

        try:
            netuid = websocket.headers.get("x-netuid")
            if netuid is None:
                return InvalidParams("Missing x-netuid header")

            if self.is_testnet:
                testnet_uids = [
                    uid[0] for uid in MAINNET_TESTNET_UIDS if uid[1] == int(netuid)
                ]
                if not testnet_uids:
                    return InvalidParams(
                        f"No testnet UID mapping found for mainnet UID {netuid}"
                    )
                netuid = testnet_uids[0]

            netuid = int(netuid)
            try:
                external_request = ProofOfWeightsRPCRequest(
                    evaluation_data=evaluation_data,
                    netuid=netuid,
                    weights_version=weights_version,
                )
            except ValueError as e:
                return InvalidParams(str(e))

            self.pending_requests[external_request.hash] = asyncio.Event()
            self.stacked_requests_queue.insert(0, external_request)
            bt.logging.success(
                f"External request with hash {external_request.hash} added to queue"
            )
            try:
                await asyncio.wait_for(
                    self.pending_requests[external_request.hash].wait(),
                    timeout=external_request.circuit.timeout
                    + EXTERNAL_REQUEST_QUEUE_TIME_SECONDS,
                )
                result = self.request_results.pop(external_request.hash, None)

                if result["success"]:
                    bt.logging.success(
                        f"External request with hash {external_request.hash} processed successfully"
                    )
                    return Success(result)
                bt.logging.error(
                    f"External request with hash {external_request.hash} failed to process"
                )
                return Error(9, "Request processing failed")
            except asyncio.TimeoutError:
                bt.logging.error(
                    f"External request with hash {external_request.hash} timed out"
                )
                return Error(9, "Request processing failed", "Request timed out")
            finally:
                self.pending_requests.pop(external_request.hash, None)

        except Exception as e:
            bt.logging.error(f"Error processing request: {str(e)}")
            traceback.print_exc()
            return Error(9, "Request processing failed", str(e))

    async def handle_proof_of_computation(
        self, websocket: WebSocket, **params: dict[str, object]
    ) -> dict[str, object]:
        input_json = params.get("input")
        circuit_id = params.get("circuit")

        if not input_json:
            return InvalidParams("Missing input to the circuit")

        if not circuit_id:
            return InvalidParams("Missing circuit id")

        try:
            try:
                external_request = ProofOfComputationRPCRequest(
                    circuit_id=circuit_id,
                    inputs=input_json,
                )
            except ValueError as e:
                bt.logging.error(
                    f"Error creating proof of computation request: {str(e)}"
                )
                return InvalidParams(str(e))

            self.pending_requests[external_request.hash] = asyncio.Event()
            self.stacked_requests_queue.insert(0, external_request)
            bt.logging.success(
                f"External request with hash {external_request.hash} added to queue"
            )
            try:
                await asyncio.wait_for(
                    self.pending_requests[external_request.hash].wait(),
                    timeout=external_request.circuit.timeout
                    + EXTERNAL_REQUEST_QUEUE_TIME_SECONDS,
                )
                result = self.request_results.pop(external_request.hash, None)

                if result["success"]:
                    bt.logging.success(
                        f"External request with hash {external_request.hash} processed successfully"
                    )
                    return Success(result)
                bt.logging.error(
                    f"External request with hash {external_request.hash} failed to process"
                )
                return Error(9, "Request processing failed")
            except asyncio.TimeoutError:
                bt.logging.error(
                    f"External request with hash {external_request.hash} timed out"
                )
                return Error(9, "Request processing failed", "Request timed out")
            finally:
                self.pending_requests.pop(external_request.hash, None)

        except Exception as e:
            bt.logging.error(f"Error processing request: {str(e)}")
            traceback.print_exc()
            return Error(9, "Request processing failed", str(e))

    def start_server(self):
        """Start the uvicorn server in a separate thread"""
        self.server_thread = threading.Thread(
            target=uvicorn.run,
            args=(app,),
            kwargs={
                "host": "0.0.0.0",
                "port": self.config.api.port,
                "ssl_keyfile": os.path.join(
                    self.config.api.certificate_path, "key.pem"
                ),
                "ssl_certfile": os.path.join(
                    self.config.api.certificate_path, "cert.pem"
                ),
            },
            daemon=True,
        )
        self.server_thread.start()
        if not self.config.api.serve_axon:
            return
        try:
            bt.logging.info(f"Serving axon on port {self.config.api.port}")
            axon = bt.axon(
                wallet=self.config.wallet, external_port=self.config.api.port
            )
            existing_axon = self.config.metagraph.axons[self.config.user_uid]
            if (
                existing_axon
                and existing_axon.port == axon.external_port
                and existing_axon.ip == axon.external_ip
            ):
                bt.logging.debug(
                    f"Axon already serving on ip {axon.external_ip} and port {axon.external_port}"
                )
                return
            axon.serve(self.config.bt_config.netuid, self.config.subtensor)
            bt.logging.success("Axon served")
        except Exception as e:
            bt.logging.error(f"Error serving axon: {e}")

    async def stop(self):
        """Gracefully shutdown the WebSocket server"""
        for connection in self.ws_manager.active_connections:
            await connection.close()
        self.ws_manager.active_connections.clear()

    async def validate_connection(self, headers) -> bool:
        required_headers = ["x-timestamp", "x-origin-ss58", "x-signature"]
        if not all(header in headers for header in required_headers):
            bt.logging.warning(
                f"Incoming request is missing required headers: {required_headers}"
            )
            return False

        try:
            timestamp = int(headers["x-timestamp"])
            current_time = time.time()
            if current_time - timestamp > MAX_SIGNATURE_LIFESPAN:
                bt.logging.warning(
                    f"Incoming request signature timestamp {timestamp} is too old. Current time: {current_time}"
                )
                return False

            ss58_address = headers["x-origin-ss58"]
            signature = base64.b64decode(headers["x-signature"])

            public_key = substrateinterface.Keypair(ss58_address=ss58_address)
            if not public_key.verify(str(timestamp).encode(), signature):
                bt.logging.warning(
                    f"Incoming request signature verification failed for address {ss58_address}"
                )
                return False

            if "x-netuid" in headers:
                netuid = int(headers["x-netuid"])
                return await self.validator_keys_cache.check_validator_key(
                    ss58_address, netuid
                )
            else:
                return await self.validator_keys_cache.check_whitelisted_key(
                    ss58_address
                )

        except Exception as e:
            bt.logging.error(f"Validation error: {str(e)}")
            traceback.print_exc()
            return False

    def commit_cert_hash(self):
        """Commit the cert hash to the chain. Clients will use this for certificate pinning."""

        existing_commitment = None
        try:
            existing_commitment = self.config.subtensor.get_commitment(
                self.config.subnet_uid, self.config.user_uid
            )
        except Exception:
            bt.logging.warning(
                "Error getting existing commitment. Assuming no commitment exists."
            )
            traceback.print_exc()

        if not self.config.api.certificate_path:
            return

        cert_path = os.path.join(self.config.api.certificate_path, "cert.pem")
        if not os.path.exists(cert_path):
            return

        with open(cert_path, "rb") as f:
            cert_data = f.read()
            cert = crypto.load_certificate(crypto.FILETYPE_PEM, cert_data)
            cert_der = crypto.dump_certificate(crypto.FILETYPE_ASN1, cert)
            cert_hash = hashlib.sha256(cert_der).hexdigest()
            if cert_hash != existing_commitment:
                try:
                    self.config.subtensor.commit(
                        self.config.wallet, self.config.subnet_uid, cert_hash
                    )
                    bt.logging.success("Certificate hash committed to chain.")
                except Exception as e:
                    bt.logging.error(f"Error committing certificate hash: {str(e)}")
                    traceback.print_exc()
            else:
                bt.logging.debug("Certificate hash already committed to chain.")

    def set_request_result(self, request_hash: str, result: dict[str, any]):
        """Set the result for a pending request and signal its completion."""
        if request_hash in self.pending_requests:
            self.request_results[request_hash] = result
            self.pending_requests[request_hash].set()
