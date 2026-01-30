from __future__ import annotations

import asyncio
import base64
import json
import traceback
from pathlib import Path

import bittensor as bt
import websockets
from deployment_layer.circuit_store import circuit_store
from execution_layer.circuit import ProofSystem
from execution_layer.dsperse_manager import DSperseManager, DsperseRun
from jsonrpcserver import (
    Error,
    InvalidParams,
    Success,
    async_dispatch,
)
from websockets.exceptions import ConnectionClosed

from _validator.config import ValidatorConfig
from _validator.models.base_rpc_request import QueuedRequestDataModel
from _validator.models.poc_rpc_request import ProofOfComputationRPCRequest
from constants import (
    EXTERNAL_REQUEST_QUEUE_TIME_SECONDS,
    RELAY_AUTH_TIMEOUT,
    RELAY_OPEN_TIMEOUT,
    RELAY_PING_INTERVAL,
    RELAY_PING_TIMEOUT,
    RELAY_RECONNECT_BASE_DELAY,
    RELAY_RECONNECT_MAX_DELAY,
)


class AuthenticationError(Exception):
    """Raised when authentication with the relay fails."""

    pass


class RelayManager:
    """
    WebSocket client that connects to the SN2 Relay service.

    Maintains a persistent connection, handles authentication,
    processes incoming JSON-RPC requests, and sends batch completion
    notifications with generated proofs.
    """

    def __init__(self, config: ValidatorConfig):
        self.config = config
        # Queue of requests to be sent to miners (consumed by ValidatorLoop)
        self.stacked_requests_queue: list[QueuedRequestDataModel] = []
        self.pending_requests: dict[str, asyncio.Event] = {}
        self.request_results: dict[str, dict] = {}
        self.is_testnet = config.bt_config.subtensor.network == "test"
        self.active_runs: dict[str, DsperseRun] = {}
        self.dsperse_manager: DSperseManager | None = None

        # WebSocket client state
        self._ws: websockets.WebSocketClientProtocol | None = None
        self._connected = asyncio.Event()
        self._should_run = True
        self._reconnect_delay = RELAY_RECONNECT_BASE_DELAY

        # Queue for notifications to send when reconnected
        self._pending_notifications: list[dict] = []

    def start(self) -> None:
        """Start the WebSocket client connection (called from ValidatorLoop)."""
        if not self.config.relay_enabled:
            bt.logging.info(
                "Relay client disabled: --ignore-external-requests flag present"
            )
            return

        bt.logging.info("Starting SN2 Relay client...")
        asyncio.create_task(self._run())

    async def _run(self) -> None:
        """Main run loop with reconnection."""
        while self._should_run:
            try:
                await self._connect_and_handle()
            except AuthenticationError as e:
                bt.logging.error(f"Relay authentication failed: {e}")
            except ConnectionClosed as e:
                bt.logging.warning(f"Relay connection closed: {e}")
            except Exception as e:
                bt.logging.error(f"Relay connection error: {e}")
                traceback.print_exc()

            if self._should_run:
                await self._handle_reconnect()

    async def _connect_and_handle(self) -> None:
        """Connect, authenticate, and handle messages."""
        bt.logging.info(f"Connecting to SN2 Relay at {self.config.relay_url}...")

        try:
            async with websockets.connect(
                self.config.relay_url,
                open_timeout=RELAY_OPEN_TIMEOUT,
                ping_interval=RELAY_PING_INTERVAL,
                ping_timeout=RELAY_PING_TIMEOUT,
            ) as ws:
                bt.logging.debug("WebSocket handshake completed")
                self._ws = ws
                await self._authenticate(ws)
                bt.logging.success("Connected and authenticated to SN2 Relay")
                self._connected.set()
                self._reconnect_delay = RELAY_RECONNECT_BASE_DELAY

                # Send any queued notifications from previous disconnection
                await self._flush_pending_notifications()

                # Run message loop and batch monitor concurrently
                await asyncio.gather(
                    self._message_loop(ws),
                    self._batch_monitor_loop(),
                )
        except asyncio.TimeoutError as e:
            bt.logging.error(f"WS Connection timeout: {e}")
            raise

    async def _handle_reconnect(self) -> None:
        """Handle reconnection with exponential backoff."""
        self._connected.clear()
        self._ws = None
        bt.logging.warning(f"Reconnecting to SN2 Relay in {self._reconnect_delay}s...")
        await asyncio.sleep(self._reconnect_delay)
        self._reconnect_delay = min(
            self._reconnect_delay * 2,
            RELAY_RECONNECT_MAX_DELAY,
        )

    async def _authenticate(self, ws: websockets.WebSocketClientProtocol) -> None:
        """
        Handle auth challenge/response flow per VALIDATOR_INTEGRATION.md.

        1. Receive auth_challenge with 40 bytes (32 nonce + 8 timestamp)
        2. Sign challenge bytes with validator's sr25519 keypair
        3. Send auth_response with SS58 address and signature
        4. Receive auth_success or connection close
        """
        # Receive challenge
        try:
            bt.logging.info(
                f"Authenticating with SN2 Relay (ss58:{self.config.wallet.hotkey.ss58_address})..."
            )
            raw_msg = await asyncio.wait_for(ws.recv(), timeout=RELAY_AUTH_TIMEOUT)
        except asyncio.TimeoutError:
            bt.logging.error("Timeout waiting for auth_challenge from relay")
            raise AuthenticationError("Timeout waiting for auth_challenge")
        msg = json.loads(raw_msg)

        if msg.get("type") != "auth_challenge":
            raise AuthenticationError(f"Expected auth_challenge, got {msg.get('type')}")

        challenge_bytes = base64.b64decode(msg["challenge"])

        # Sign with validator's sr25519 keypair
        signature = self.config.wallet.hotkey.sign(challenge_bytes)

        # Send response
        await ws.send(
            json.dumps(
                {
                    "type": "auth_response",
                    "ss58": self.config.wallet.hotkey.ss58_address,
                    "signature": base64.b64encode(signature).decode(),
                }
            )
        )

        # Wait for success
        raw_result = await asyncio.wait_for(ws.recv(), timeout=RELAY_AUTH_TIMEOUT)
        result = json.loads(raw_result)

        if result.get("type") != "auth_success":
            raise AuthenticationError("Authentication failed")

    async def _message_loop(self, ws: websockets.WebSocketClientProtocol) -> None:
        """Handle incoming JSON-RPC messages from relay."""
        async for message in ws:
            bt.logging.debug(f"Received relay message: {message[:100]}...")
            try:
                response = await async_dispatch(
                    message,
                    methods={
                        "subnet-2.proof_of_computation": self.handle_proof_of_computation,
                        "subnet-2.dsperse_submit": self.handle_dsperse_submit,
                        "subnet-2.run_status": self.handle_run_status,
                    },
                )
                await ws.send(str(response))
            except Exception as e:
                bt.logging.error(f"Error processing relay message: {e}")
                traceback.print_exc()
                # Send error response
                error_response = json.dumps(
                    {
                        "jsonrpc": "2.0",
                        "error": {"code": -32603, "message": str(e)},
                        "id": None,
                    }
                )
                await ws.send(error_response)

    async def _batch_monitor_loop(self) -> None:
        """Monitor batches and send completion notifications."""
        while self._should_run and self._ws:
            try:
                for batch_id, batch_run in list(self.active_runs.items()):
                    if self._is_batch_complete(batch_run):
                        await self._send_batch_completion(batch_id, batch_run)
            except Exception as e:
                bt.logging.error(f"Error in batch monitor: {e}")
                traceback.print_exc()
            await asyncio.sleep(1)

    def _is_batch_complete(self, batch_run: DsperseRun) -> bool:
        """Check if all slices in a batch are complete (success or failed)."""
        if not self.dsperse_manager:
            return False
        progress = self.dsperse_manager.get_run_status(batch_run.run_uid)
        total = progress["total_slices"]
        done = progress["completed"] + progress["failed"]
        return total > 0 and done >= total

    async def _send_batch_completion(
        self, batch_id: str, batch_run: DsperseRun
    ) -> None:
        """Send batch completion notification with proofs."""
        proofs = self._collect_batch_proofs(batch_run)
        progress = self.dsperse_manager.get_run_status(batch_run.run_uid)

        notification = {
            "jsonrpc": "2.0",
            "method": "subnet-2.batch_completed",
            "params": {
                "batch_id": batch_id,
                "status": (
                    "completed" if not batch_run.failed else "completed_with_errors"
                ),
                "proofs": proofs,
                "progress": progress,
            },
        }

        if self._ws and self._connected.is_set():
            try:
                await self._ws.send(json.dumps(notification))
                bt.logging.success(f"Sent batch completion for {batch_id}")
            except Exception as e:
                bt.logging.warning(f"Failed to send completion, queueing: {e}")
                self._pending_notifications.append(notification)
        else:
            # Queue for when we reconnect
            bt.logging.info(f"Queueing batch completion for {batch_id} (disconnected)")
            self._pending_notifications.append(notification)

        self._cleanup_batch(batch_id, batch_run)

    async def _flush_pending_notifications(self) -> None:
        """Send any queued notifications after reconnecting."""
        while self._pending_notifications and self._ws:
            notification = self._pending_notifications.pop(0)
            try:
                await self._ws.send(json.dumps(notification))
                bt.logging.info(
                    f"Sent queued notification: {notification.get('method')}"
                )
            except Exception as e:
                bt.logging.error(f"Failed to send queued notification: {e}")
                self._pending_notifications.insert(0, notification)
                break

    def _collect_batch_proofs(self, batch_run: DsperseRun) -> list[dict]:
        """Collect all generated proofs from a completed batch."""
        proofs = []
        for frame_idx, frame_result in batch_run.frame_results.items():
            if not frame_result.success:
                continue
            for slice_data in frame_result.slices:
                if slice_data.proof_file and slice_data.proof_file.exists():
                    proof_content = self._read_proof_file(
                        slice_data.proof_file, slice_data.proof_system
                    )
                    if proof_content is not None:
                        proofs.append(
                            {
                                "frame_idx": frame_idx,
                                "slice_num": slice_data.slice_num,
                                "proof_system": slice_data.proof_system.value,
                                "proof": proof_content,
                            }
                        )
        return proofs

    def _read_proof_file(
        self, proof_file: Path, proof_system: ProofSystem
    ) -> dict | str | None:
        """Read proof content from file based on proof system."""
        try:
            if proof_system == ProofSystem.JSTPROVE:
                with open(proof_file, "rb") as f:
                    return f.read().hex()
            else:
                with open(proof_file, "r") as f:
                    return json.load(f)
        except Exception as e:
            bt.logging.error(f"Error reading proof file {proof_file}: {e}")
            return None

    def _cleanup_batch(self, batch_id: str, batch_run: DsperseRun) -> None:
        """Clean up a completed batch."""
        del self.active_runs[batch_id]
        if self.dsperse_manager:
            for frame_result in batch_run.frame_results.values():
                try:
                    self.dsperse_manager.cleanup_run(frame_result.run_uid)
                except ValueError:
                    bt.logging.debug(
                        f"Run {frame_result.run_uid} already cleaned up or not found"
                    )
        bt.logging.info(f"Batch {batch_id} completed and cleaned up")

    async def handle_proof_of_computation(self, **params: dict) -> dict:
        """
        Handle subnet-2.proof_of_computation RPC method.

        Queues a proof generation request and waits for completion.
        """
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

                if result and result.get("success"):
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

    async def handle_dsperse_submit(
        self, **params: dict[str, object]
    ) -> dict[str, object]:
        circuit_id = params.get("circuit_id")
        inputs = params.get("inputs")

        if not circuit_id:
            return InvalidParams("Missing circuit_id")
        if not inputs or not isinstance(inputs, dict):
            return InvalidParams("Missing or invalid inputs")

        try:
            circuit = circuit_store.ensure_circuit(circuit_id)
            if not self.dsperse_manager:
                return Error(10, "DSperse manager not initialized")

            bt.logging.info(f"Starting DSperse run for circuit {circuit_id}")

            loop = asyncio.get_event_loop()
            run_uid, requests = await loop.run_in_executor(
                None,
                lambda: self.dsperse_manager.start_run(circuit, inputs),
            )

            for request in requests:
                self.stacked_requests_queue.append(request)

            status = self.dsperse_manager.get_run_status(run_uid)
            bt.logging.success(
                f"Run {run_uid} created: {status['total_slices']} slices queued"
            )

            return Success(
                {"run_uid": run_uid, "status": "processing", "progress": status}
            )

        except Exception as e:
            bt.logging.error(f"Error in DSperse submission: {str(e)}")
            traceback.print_exc()
            return Error(9, "DSperse submission failed", str(e))

    async def handle_run_status(self, **params: dict[str, object]) -> dict[str, object]:
        run_uid = params.get("run_uid")

        if not run_uid:
            return InvalidParams("Missing run_uid")

        if not self.dsperse_manager:
            return Error(10, "DSperse manager not initialized")

        status = self.dsperse_manager.get_run_status(run_uid)
        if not status:
            return Error(11, "Run not found", f"No run with ID {run_uid}")

        try:
            if status["is_complete"]:
                run_status = (
                    "completed" if status["all_successful"] else "completed_with_errors"
                )
                self._cleanup_run(run_uid)
            else:
                run_status = "processing"

            return Success(
                {"run_uid": run_uid, "status": run_status, "progress": status}
            )

        except Exception as e:
            bt.logging.error(f"Error getting run status: {str(e)}")
            traceback.print_exc()
            return Error(9, "Failed to get run status", str(e))

    def _cleanup_run(self, run_uid: str) -> None:
        if self.dsperse_manager:
            try:
                self.dsperse_manager.cleanup_run(run_uid)
            except ValueError:
                bt.logging.debug(f"Run {run_uid} already cleaned up or not found")
        bt.logging.info(f"Run {run_uid} completed and cleaned up")

    async def stop(self) -> None:
        """Gracefully shutdown the WebSocket client."""
        bt.logging.info("Stopping SN2 Relay client...")
        self._should_run = False
        self._connected.clear()
        if self._ws:
            try:
                await self._ws.close()
            except Exception as e:
                bt.logging.debug(f"Error closing websocket: {e}")
        self._ws = None

    def set_request_result(self, request_hash: str, result: dict) -> None:
        """Set the result for a pending request and signal its completion."""
        if request_hash in self.pending_requests:
            self.request_results[request_hash] = result
            self.pending_requests[request_hash].set()
