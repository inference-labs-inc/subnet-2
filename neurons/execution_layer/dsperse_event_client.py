import asyncio
import base64
import json
import logging
from datetime import datetime, timezone
from typing import Any

import httpx

logger = logging.getLogger(__name__)

BATCH_SIZE = 20
FLUSH_INTERVAL = 0.5


class DsperseEventClient:
    def __init__(self, api_url: str, wallet):
        self.api_url = api_url.rstrip("/")
        self.wallet = wallet
        self._buffer: list[dict] = []
        self._buffer_lock = asyncio.Lock()
        self._task: asyncio.Task | None = None
        self._running = False
        self._client: httpx.AsyncClient | None = None

    def start(self):
        if self._task is None or self._task.done():
            try:
                loop = asyncio.get_running_loop()
                self._running = True
                self._client = httpx.AsyncClient(timeout=10.0)
                self._task = loop.create_task(self._flush_loop())
            except RuntimeError:
                self._running = False

    def stop(self):
        self._running = False
        if self._task is not None:
            self._task.cancel()
        if self._client:
            try:
                loop = asyncio.get_running_loop()
                loop.create_task(self._client.aclose())
            except RuntimeError:
                pass

    async def emit(self, event: dict[str, Any]):
        if "timestamp" not in event:
            event["timestamp"] = datetime.now(timezone.utc).isoformat()
        event["validator_key"] = self.wallet.hotkey.ss58_address

        should_flush = False
        async with self._buffer_lock:
            self._buffer.append(event)
            if len(self._buffer) >= BATCH_SIZE:
                should_flush = True

        if should_flush:
            await self._flush()

    async def emit_run_started(
        self,
        run_uid: str,
        circuit_id: str,
        circuit_name: str | None,
        total_slices: int,
        environment: dict | None,
    ):
        await self.emit(
            {
                "event_type": "run_started",
                "run_uid": run_uid,
                "circuit_id": circuit_id,
                "circuit_name": circuit_name,
                "total_slices": total_slices,
                "environment": environment,
            }
        )

    async def emit_witness_complete(
        self,
        run_uid: str,
        slice_num: str,
        witness_time_sec: float,
        memory_peak_mb: float | None = None,
    ):
        await self.emit(
            {
                "event_type": "witness_complete",
                "run_uid": run_uid,
                "slice_num": slice_num,
                "witness_time_sec": witness_time_sec,
                "memory_peak_mb": memory_peak_mb,
            }
        )

    async def emit_proof_received(
        self,
        run_uid: str,
        slice_num: str,
        response_time_sec: float,
        miner_uid: int | None = None,
    ):
        await self.emit(
            {
                "event_type": "proof_received",
                "run_uid": run_uid,
                "slice_num": slice_num,
                "response_time_sec": response_time_sec,
                "miner_uid": miner_uid,
            }
        )

    async def emit_verification_complete(
        self,
        run_uid: str,
        slice_num: str,
        verification_time_sec: float,
        success: bool,
    ):
        await self.emit(
            {
                "event_type": "verification_complete",
                "run_uid": run_uid,
                "slice_num": slice_num,
                "verification_time_sec": verification_time_sec,
                "success": success,
            }
        )

    async def emit_slice_failed(
        self,
        run_uid: str,
        slice_num: str,
        error: str | None = None,
    ):
        await self.emit(
            {
                "event_type": "slice_failed",
                "run_uid": run_uid,
                "slice_num": slice_num,
                "success": False,
                "error": error,
            }
        )

    async def emit_run_complete(
        self,
        run_uid: str,
        all_successful: bool,
        total_run_time_sec: float | None = None,
    ):
        await self.emit(
            {
                "event_type": "run_complete",
                "run_uid": run_uid,
                "all_successful": all_successful,
                "total_run_time_sec": total_run_time_sec,
            }
        )

    async def _flush_loop(self):
        while self._running:
            try:
                await asyncio.sleep(FLUSH_INTERVAL)
                await self._flush()
            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.warning(f"Flush loop error: {e}")

    async def _flush(self):
        if not self._client:
            return

        async with self._buffer_lock:
            if not self._buffer:
                return
            events = self._buffer
            self._buffer = []

        try:
            batch = {
                "validator_key": self.wallet.hotkey.ss58_address,
                "events": events,
            }
            body = json.dumps(batch)
            signature = base64.b64encode(
                self.wallet.hotkey.sign(body.encode())
            ).decode()

            response = await self._client.post(
                f"{self.api_url}/statistics/dsperse/events/",
                content=body,
                headers={
                    "Content-Type": "application/json",
                    "X-Request-Signature": signature,
                },
            )
            if response.status_code >= 400:
                logger.warning(
                    f"Failed to flush dsperse events: {response.status_code} {response.text}"
                )
                async with self._buffer_lock:
                    self._buffer = events + self._buffer
        except Exception as e:
            logger.warning(f"Failed to flush dsperse events: {e}")
            async with self._buffer_lock:
                self._buffer = events + self._buffer
