from __future__ import annotations

import asyncio
import base64
import concurrent.futures
import contextlib
import copy
import io
import json
import multiprocessing as mp
import os
import queue
import threading
import time
import traceback

import bittensor as bt
import numpy as np
import onnxruntime as ort
import websockets
from deployment_layer.circuit_store import circuit_store
from execution_layer.dsperse_manager import DSperseManager
from execution_layer.proof_uploader import (
    upload_final_output,
    upload_input_frame,
    upload_run_proofs,
)
from jsonrpcserver import (
    Error,
    InvalidParams,
    Success,
    async_dispatch,
)

from _validator.api.inference_pb2 import InferenceRequest
from _validator.config import ValidatorConfig
from _validator.models.poc_rpc_request import ProofOfComputationRPCRequest
from _validator.models.request_type import RequestType
from constants import (
    CIRCUIT_API_URL,
    EXTERNAL_REQUEST_QUEUE_TIME_SECONDS,
    RELAY_AUTH_TIMEOUT,
    RELAY_OPEN_TIMEOUT,
    RELAY_RECONNECT_BASE_DELAY,
    RELAY_RECONNECT_MAX_DELAY,
    RunSource,
)


def _decode_protobuf_input(data: bytes) -> np.ndarray:
    msg = InferenceRequest()
    msg.ParseFromString(data)
    return np.array(msg.data, dtype=np.float32).reshape(msg.shape)


_spawn_ctx = mp.get_context("spawn")

_ONNX_OUTPUT_TTL_SEC = 600

_CLASS_NAMES = {0: "football", 1: "goalkeeper", 2: "player", 3: "referee"}


def _yolo_nms(
    output: np.ndarray,
    img_h: int = 1,
    img_w: int = 1,
    conf_threshold=0.25,
    iou_threshold=0.45,
):
    if output.ndim == 3:
        output = output[0]
    predictions = output.T
    cx, cy, w, h = (
        predictions[:, 0],
        predictions[:, 1],
        predictions[:, 2],
        predictions[:, 3],
    )
    class_probs = predictions[:, 4:]
    max_conf = class_probs.max(axis=1)
    mask = max_conf >= conf_threshold
    cx, cy, w, h = cx[mask], cy[mask], w[mask], h[mask]
    class_probs = class_probs[mask]
    max_conf = max_conf[mask]
    class_ids = class_probs.argmax(axis=1)
    x1 = cx - w / 2
    y1 = cy - h / 2
    x2 = cx + w / 2
    y2 = cy + h / 2
    areas = w * h
    order = max_conf.argsort()[::-1]
    keep = []
    while order.size > 0:
        i = order[0]
        keep.append(i)
        xx1 = np.maximum(x1[i], x1[order[1:]])
        yy1 = np.maximum(y1[i], y1[order[1:]])
        xx2 = np.minimum(x2[i], x2[order[1:]])
        yy2 = np.minimum(y2[i], y2[order[1:]])
        inter = np.maximum(0, xx2 - xx1) * np.maximum(0, yy2 - yy1)
        iou = inter / (areas[i] + areas[order[1:]] - inter)
        order = order[1:][iou <= iou_threshold]
    return [
        {
            "x": float(cx[i]) / img_w,
            "y": float(cy[i]) / img_h,
            "w": float(w_val) / img_w,
            "h": float(h_val) / img_h,
            "confidence": float(max_conf[i]),
            "class_id": int(class_ids[i]),
            "class_name": _CLASS_NAMES.get(int(class_ids[i]), str(int(class_ids[i]))),
        }
        for i, w_val, h_val in [(k, w[k], h[k]) for k in keep]
    ]


def _encode_nchw_to_jpeg(arr: np.ndarray) -> bytes:
    from PIL import Image

    img = arr[0].transpose(1, 2, 0)
    if img.max() <= 1.0:
        img = (img * 255).clip(0, 255)
    img = img.astype(np.uint8)
    if img.shape[2] == 1:
        img = img[:, :, 0]
    buf = io.BytesIO()
    Image.fromarray(img).save(buf, format="JPEG", quality=85)
    return buf.getvalue()


def _onnx_inference_worker(
    model_path: str, input_data, run_uid: str, result_queue: mp.Queue
):
    try:
        if isinstance(input_data, np.ndarray):
            arr = (
                input_data.astype(np.float32)
                if input_data.dtype != np.float32
                else input_data
            )
        else:
            arr = np.array(input_data, dtype=np.float32)
        if arr.ndim == 3:
            arr = np.expand_dims(arr, 0)
        sess = ort.InferenceSession(model_path)
        input_name = sess.get_inputs()[0].name
        result = sess.run(None, {input_name: arr})
        img_h, img_w = arr.shape[2], arr.shape[3]
        detections = _yolo_nms(result[0], img_h=img_h, img_w=img_w)
        frame_jpeg = _encode_nchw_to_jpeg(arr)
        result_queue.put((run_uid, detections, frame_jpeg))
    except Exception as exc:
        result_queue.put((run_uid, None, str(exc)))


def _relay_ws_process(
    relay_url: str,
    wallet_name: str,
    wallet_hotkey: str,
    inbox: mp.Queue,
    outbox: mp.Queue,
):
    import logging
    import signal
    import sys

    import bittensor as bt

    logging.basicConfig(
        stream=sys.stderr,
        level=logging.INFO,
        format="%(levelname)s:%(name)s:%(message)s",
    )
    log = logging.getLogger("relay_ws")

    signal.signal(signal.SIGINT, signal.SIG_IGN)
    wallet = bt.Wallet(name=wallet_name, hotkey=wallet_hotkey)

    async def _run():
        reconnect_delay = RELAY_RECONNECT_BASE_DELAY
        while True:
            try:
                async with websockets.connect(
                    relay_url,
                    open_timeout=RELAY_OPEN_TIMEOUT,
                    ping_interval=20,
                    ping_timeout=None,
                    max_size=100 * 1024 * 1024,
                ) as ws:
                    raw_msg = await asyncio.wait_for(
                        ws.recv(), timeout=RELAY_AUTH_TIMEOUT
                    )
                    msg = json.loads(raw_msg)
                    if msg.get("type") != "auth_challenge":
                        continue
                    challenge_bytes = base64.b64decode(msg["challenge"], validate=True)
                    signature = wallet.hotkey.sign(challenge_bytes)
                    await ws.send(
                        json.dumps(
                            {
                                "type": "auth_response",
                                "ss58": wallet.hotkey.ss58_address,
                                "signature": base64.b64encode(signature).decode(),
                            }
                        )
                    )
                    raw_result = await asyncio.wait_for(
                        ws.recv(), timeout=RELAY_AUTH_TIMEOUT
                    )
                    if json.loads(raw_result).get("type") != "auth_success":
                        continue

                    log.info("Relay WS process: connected and authenticated")
                    reconnect_delay = RELAY_RECONNECT_BASE_DELAY

                    async def _reader():
                        async for message in ws:
                            inbox.put(message)

                    async def _writer():
                        loop = asyncio.get_running_loop()
                        while ws.close_code is None:
                            try:
                                msg = await asyncio.wait_for(
                                    loop.run_in_executor(
                                        None, lambda: outbox.get(timeout=0.5)
                                    ),
                                    timeout=2.0,
                                )
                                await ws.send(msg)
                            except (asyncio.TimeoutError, queue.Empty):
                                continue

                    done, pending = await asyncio.wait(
                        {
                            asyncio.create_task(_reader()),
                            asyncio.create_task(_writer()),
                        },
                        return_when=asyncio.FIRST_COMPLETED,
                    )
                    for t in pending:
                        t.cancel()
                    await asyncio.gather(*pending, return_exceptions=True)

            except Exception as e:
                log.warning(f"Relay WS process error: {e}")

            log.info(f"Relay WS process: reconnecting in {reconnect_delay}s")
            await asyncio.sleep(reconnect_delay)
            reconnect_delay = min(reconnect_delay * 2, RELAY_RECONNECT_MAX_DELAY)

    asyncio.run(_run())


class AuthenticationError(Exception):
    pass


class RelayManager:
    def __init__(self, config: ValidatorConfig):
        self.config = config
        self.stacked_requests_queue: asyncio.Queue = asyncio.Queue()
        self.rwr_queue: asyncio.Queue = asyncio.Queue()
        self.pending_requests: dict[str, asyncio.Event] = {}
        self.request_results: dict[str, dict] = {}
        self.is_testnet = config.bt_config.subtensor.network == "test"
        self.dsperse_manager: DSperseManager | None = None
        self.dispatch_event: asyncio.Event | None = None
        self._onnx_outputs: dict[str, tuple[float, list]] = {}
        self._onnx_lock = threading.Lock()
        self._onnx_result_queue: mp.Queue = _spawn_ctx.Queue()
        self._onnx_processes: dict[str, mp.Process] = {}
        self._onnx_circuit_ids: dict[str, str] = {}
        self._onnx_max_concurrent = 2
        self._onnx_max_queue = 15
        self._onnx_pending: list[tuple[str, object, dict]] = []
        self._onnx_pending_lock = threading.Lock()
        self._relay_executor = concurrent.futures.ThreadPoolExecutor(max_workers=16)
        self._dsperse_submit_sem: asyncio.Semaphore | None = None
        self._background_tasks: set[asyncio.Task] = set()
        threading.Thread(target=self._monitor_onnx_results, daemon=True).start()

        self._ws_inbox: mp.Queue = _spawn_ctx.Queue()
        self._ws_outbox: mp.Queue = _spawn_ctx.Queue()
        self._ws_process: mp.Process | None = None
        self._should_run = True
        self._task: asyncio.Task | None = None

    def _monitor_onnx_results(self) -> None:
        while True:
            try:
                msg = self._onnx_result_queue.get()
                run_uid = msg[0]
                if msg[1] is None:
                    err = msg[2]
                    bt.logging.warning(
                        f"Full-model ONNX failed for run {run_uid}: {err}"
                    )
                else:
                    output = msg[1]
                    frame_jpeg = msg[2] if len(msg) > 2 else None
                    with self._onnx_lock:
                        self._onnx_outputs[run_uid] = (time.monotonic(), output)
                    bt.logging.info(f"Full-model ONNX complete for run {run_uid}")

                    circuit_id = self._onnx_circuit_ids.pop(run_uid, None)
                    if output and circuit_id:
                        threading.Thread(
                            target=self._upload_onnx_output,
                            args=(run_uid, circuit_id, output, frame_jpeg),
                            daemon=True,
                        ).start()

                self._onnx_processes.pop(run_uid, None)
                self._drain_onnx_pending()
            except Exception as e:
                bt.logging.warning(f"ONNX result monitor error: {e}")

    def _upload_onnx_output(
        self,
        run_uid: str,
        circuit_id: str,
        output: list,
        frame_jpeg: bytes | None = None,
    ) -> None:
        try:
            upload_final_output(run_uid, circuit_id, {"detections": output})
            bt.logging.info(f"Uploaded ONNX output for run {run_uid}")
        except Exception as e:
            bt.logging.warning(f"Failed to upload ONNX output for {run_uid}: {e}")
        if frame_jpeg:
            try:
                upload_input_frame(run_uid, frame_jpeg)
                bt.logging.info(f"Uploaded input frame for run {run_uid}")
            except Exception as e:
                bt.logging.warning(f"Failed to upload input frame for {run_uid}: {e}")

    def _evict_stale_onnx_outputs(self) -> None:
        cutoff = time.monotonic() - _ONNX_OUTPUT_TTL_SEC
        with self._onnx_lock:
            stale = [k for k, (ts, _) in self._onnx_outputs.items() if ts < cutoff]
            for k in stale:
                del self._onnx_outputs[k]

    def _start_ws_process(self) -> None:
        self._ws_process = _spawn_ctx.Process(
            target=_relay_ws_process,
            args=(
                self.config.relay_url,
                self.config.wallet.name,
                self.config.wallet.hotkey_str,
                self._ws_inbox,
                self._ws_outbox,
            ),
            daemon=True,
        )
        self._ws_process.start()

    def start(self) -> None:
        if not self.config.relay_enabled:
            bt.logging.info(
                "Relay client disabled: --ignore-external-requests flag present"
            )
            return

        bt.logging.info("Starting SN2 Relay (spawn WS process)...")
        self._start_ws_process()
        self._task = asyncio.create_task(self._dispatch_loop())
        self._task.add_done_callback(self._on_task_done)

    def _on_task_done(self, task: asyncio.Task) -> None:
        if task.cancelled():
            return
        exc = task.exception()
        if exc:
            bt.logging.error(f"Relay dispatch loop failed: {exc}")

    def _check_ws_process_health(self) -> None:
        if self._ws_process and not self._ws_process.is_alive():
            exit_code = self._ws_process.exitcode
            bt.logging.warning(
                f"Relay WS process died (exit code {exit_code}), restarting..."
            )
            self._ws_process.close()
            self._start_ws_process()

    async def _dispatch_loop(self) -> None:
        if self._dsperse_submit_sem is None:
            self._dsperse_submit_sem = asyncio.Semaphore(64)
        loop = asyncio.get_running_loop()
        health_check_counter = 0
        while self._should_run:
            try:
                message = await loop.run_in_executor(
                    self._relay_executor,
                    lambda: self._ws_inbox.get(timeout=1.0),
                )
            except queue.Empty:
                health_check_counter += 1
                if health_check_counter >= 10:
                    health_check_counter = 0
                    self._check_ws_process_health()
                continue

            bt.logging.debug(f"Received relay message: {str(message)[:100]}...")
            task = asyncio.create_task(self._handle_relay_message(message))
            self._background_tasks.add(task)
            task.add_done_callback(self._background_tasks.discard)

    async def _handle_relay_message(self, message: str) -> None:
        try:
            response = await async_dispatch(
                message,
                methods={
                    "subnet-2.proof_of_computation": self.handle_proof_of_computation,
                    "subnet-2.dsperse_submit": self._guarded_dsperse_submit,
                    "subnet-2.run_status": self.handle_run_status,
                },
            )
            self._ws_outbox.put(str(response))
        except Exception as e:
            bt.logging.error(f"Error processing relay message: {e}")
            traceback.print_exc()
            request_id = None
            with contextlib.suppress(Exception):
                request_id = json.loads(message).get("id")
            self._ws_outbox.put(
                json.dumps(
                    {
                        "jsonrpc": "2.0",
                        "error": {"code": -32603, "message": str(e)},
                        "id": request_id,
                    }
                )
            )

    async def _guarded_dsperse_submit(self, **params: object) -> dict[str, object]:
        if self._dsperse_submit_sem.locked():
            bt.logging.warning("DSperse submit queue full (max 64)")
            return Error(20, "Server busy, try again later")
        async with self._dsperse_submit_sem:
            return await self.handle_dsperse_submit(**params)

    def on_api_run_complete(
        self,
        run_uid: str,
        circuit_id: str,
        success: bool,
        proof_artifacts: list[dict],
    ) -> None:
        if proof_artifacts:
            circuit = circuit_store.circuits.get(circuit_id)
            circuit_name = circuit.metadata.name if circuit else circuit_id
            threading.Thread(
                target=upload_run_proofs,
                args=(run_uid, circuit_id, circuit_name, proof_artifacts),
                daemon=True,
            ).start()

        self._ws_outbox.put(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "method": "subnet-2.batch_completed",
                    "params": {
                        "run_uid": run_uid,
                        "circuit_id": circuit_id,
                        "status": "completed" if success else "completed_with_errors",
                    },
                }
            )
        )

    async def handle_proof_of_computation(self, **params: dict) -> dict:
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

            if external_request.circuit.metadata.input_schema:
                try:
                    external_request.circuit.input_handler(
                        RequestType.RWR, copy.deepcopy(external_request.inputs)
                    )
                except (ValueError, TypeError) as e:
                    bt.logging.warning(
                        f"Input validation failed for circuit {circuit_id}: {e}"
                    )
                    return InvalidParams(f"Invalid input shape: {e}")

            self.pending_requests[external_request.hash] = asyncio.Event()
            self.rwr_queue.put_nowait(external_request)
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

    def _ensure_full_model(self, circuit) -> str | None:
        model_path = circuit.paths.full_model
        if os.path.exists(model_path):
            return model_path

        url = f"{CIRCUIT_API_URL}/circuits/files/{circuit.id}/full_model.onnx"
        os.makedirs(os.path.dirname(model_path), exist_ok=True)
        bt.logging.info(
            f"Downloading full ONNX model for circuit {circuit.id} to {model_path}"
        )
        try:
            circuit_store._download_file(url, model_path)
        except Exception as e:
            bt.logging.warning(f"Full ONNX model not available for {circuit.id}: {e}")
            return None
        return model_path if os.path.exists(model_path) else None

    def _spawn_onnx_process(self, run_uid: str, circuit, inputs: dict) -> None:
        model_path = self._ensure_full_model(circuit)
        if not model_path:
            return
        input_data = inputs.get("input_data")
        if input_data is None:
            return
        self._onnx_circuit_ids[run_uid] = circuit.id
        active = len([p for p in self._onnx_processes.values() if p.is_alive()])
        if active >= self._onnx_max_concurrent:
            with self._onnx_pending_lock:
                if len(self._onnx_pending) >= self._onnx_max_queue:
                    evicted = self._onnx_pending.pop(0)
                    bt.logging.info(
                        f"ONNX queue full, evicting oldest run {evicted[0]}"
                    )
                self._onnx_pending.append((run_uid, model_path, input_data))
            bt.logging.info(
                f"ONNX queued for run {run_uid} ({active} active, {len(self._onnx_pending)} pending)"
            )
            return
        self._start_onnx_process(run_uid, model_path, input_data)

    def _start_onnx_process(self, run_uid: str, model_path, input_data) -> None:
        p = _spawn_ctx.Process(
            target=_onnx_inference_worker,
            args=(model_path, input_data, run_uid, self._onnx_result_queue),
            daemon=True,
        )
        p.start()
        self._onnx_processes[run_uid] = p

    def _drain_onnx_pending(self) -> None:
        active = len([p for p in self._onnx_processes.values() if p.is_alive()])
        with self._onnx_pending_lock:
            while self._onnx_pending and active < self._onnx_max_concurrent:
                run_uid, model_path, input_data = self._onnx_pending.pop(0)
                self._start_onnx_process(run_uid, model_path, input_data)
                active += 1
                bt.logging.info(
                    f"ONNX dequeued for run {run_uid} ({active} active, {len(self._onnx_pending)} pending)"
                )

    async def handle_dsperse_submit(self, **params: object) -> dict[str, object]:
        circuit_id = params.get("circuit_id")
        inputs = params.get("inputs")

        if not circuit_id:
            return InvalidParams("Missing circuit_id")
        if not inputs or not isinstance(inputs, dict):
            return InvalidParams("Missing or invalid inputs")

        try:
            loop = asyncio.get_running_loop()
            protobuf_b64 = inputs.get("protobuf") if isinstance(inputs, dict) else None
            protobuf_array = None
            if protobuf_b64:
                raw_bytes = base64.b64decode(protobuf_b64)
                protobuf_array = await loop.run_in_executor(
                    self._relay_executor, _decode_protobuf_input, raw_bytes
                )
                inputs = await loop.run_in_executor(
                    self._relay_executor,
                    lambda: {"input_data": protobuf_array.tolist()},
                )
                bt.logging.info(
                    f"Decoded protobuf input: shape={list(protobuf_array.shape)}"
                )

            circuit = circuit_store.ensure_circuit(circuit_id)
            if not self.dsperse_manager:
                return Error(10, "DSperse manager not initialized")

            if circuit.metadata.input_schema:
                try:
                    circuit.input_handler(RequestType.RWR, copy.deepcopy(inputs))
                except (ValueError, TypeError) as e:
                    bt.logging.warning(
                        f"Input validation failed for circuit {circuit_id}: {e}"
                    )
                    return InvalidParams(f"Invalid input shape: {e}")

            bt.logging.info(f"Starting DSperse run for circuit {circuit_id}")

            run_uid = await loop.run_in_executor(
                self._relay_executor,
                lambda: self.dsperse_manager.start_incremental_run(
                    circuit, inputs, RunSource.API, max_tiles=1
                ),
            )

            if self.dispatch_event:
                self.dispatch_event.set()

            status = self.dsperse_manager._incremental_runner.get_run_status(run_uid)
            return Success(
                {
                    "run_uid": run_uid,
                    "status": "processing",
                    "total_slices": status.total_slices if status else 0,
                    "total_tiles": status.total_tiles if status else 0,
                    "slice_tile_counts": (status.slice_tile_counts if status else {}),
                }
            )

        except Exception as e:
            bt.logging.error(f"Error in DSperse submission: {str(e)}")
            traceback.print_exc()
            return Error(9, "DSperse submission failed", str(e))

    async def handle_run_status(self, **params: object) -> dict[str, object]:
        run_uid = params.get("run_uid")

        if not run_uid:
            return InvalidParams("Missing run_uid")

        if not self.dsperse_manager:
            return Error(10, "DSperse manager not initialized")

        status = self.dsperse_manager.get_run_status(run_uid)
        if not status:
            with self._onnx_lock:
                entry = self._onnx_outputs.get(run_uid)
            if entry:
                return Success(
                    {
                        "run_uid": run_uid,
                        "status": "completed",
                        "progress": {},
                        "output": entry[1],
                    }
                )
            return Error(11, "Run not found", f"No run with ID {run_uid}")

        try:
            with self._onnx_lock:
                entry = self._onnx_outputs.get(run_uid)
            onnx_output = entry[1] if entry else None

            if status.is_complete:
                run_status = (
                    "completed" if status.all_successful else "completed_with_errors"
                )
            else:
                run_status = "processing"

            response = {
                "run_uid": run_uid,
                "status": run_status,
                "progress": status.to_dict(),
            }
            if onnx_output is not None:
                response["output"] = onnx_output

            return Success(response)

        except Exception as e:
            bt.logging.error(f"Error getting run status: {str(e)}")
            traceback.print_exc()
            return Error(9, "Failed to get run status", str(e))

    def _cleanup_run(self, run_uid: str) -> None:
        proc = self._onnx_processes.pop(run_uid, None)
        if proc and proc.is_alive():
            proc.kill()
        if self.dsperse_manager:
            try:
                self.dsperse_manager.cleanup_run(run_uid)
            except ValueError:
                bt.logging.debug(f"Run {run_uid} already cleaned up or not found")
        bt.logging.info(f"Run {run_uid} completed and cleaned up")

    async def stop(self) -> None:
        bt.logging.info("Stopping SN2 Relay client...")
        self._should_run = False
        if self._task:
            self._task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self._task
        if self._ws_process and self._ws_process.is_alive():
            self._ws_process.terminate()
            self._ws_process.join(timeout=5)

    def set_request_result(self, request_hash: str, result: dict) -> None:
        if request_hash in self.pending_requests:
            self.request_results[request_hash] = result
            self.pending_requests[request_hash].set()
