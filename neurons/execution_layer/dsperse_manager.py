import asyncio
import json
import os
import random
import shutil
import tempfile
import threading
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Callable, Optional

import torch
from bittensor import logging
from deployment_layer.circuit_store import circuit_store
from dsperse.src.analyzers.schema import ExecutionInfo, ExecutionMethod, RunMetadata
import time

from dsperse.src.backends.ezkl import EZKL
from dsperse.src.backends.jstprove import JSTprove
from dsperse.src.run.runner import Runner
from dsperse.src.verify.verifier import Verifier
from execution_layer.circuit import Circuit, CircuitType, ProofSystem
import numpy as np

from execution_layer.incremental_runner import IncrementalRunner
from utils.system import capture_environment

import cli_parser
from _validator.models.dslice_request import DSliceQueuedProofRequest
from _validator.models.request_type import RequestType

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from execution_layer.dsperse_event_client import DsperseEventClient


@dataclass
class SliceTimingData:
    slice_num: str
    proof_system: Optional[str] = None
    backend_used: Optional[str] = None
    witness_time_sec: float = 0.0
    response_time_sec: float = 0.0
    verification_time_sec: float = 0.0
    is_tiled: bool = False
    tile_count: Optional[int] = None
    memory_peak_mb: float = 0.0
    success: bool = False
    error: Optional[str] = None


@dataclass
class DSliceData:
    slice_num: str
    circuit_id: str
    input_file: Path
    output_file: Path
    proof_system: ProofSystem
    witness_file: Path | None = None
    proof_file: Path | None = None
    success: bool | None = None
    timing: SliceTimingData | None = None


@dataclass
class DsperseRun:
    run_uid: str
    circuit_id: str
    run_dir: Path
    slices: dict[str, DSliceData] = field(default_factory=dict)
    pending: set[str] = field(default_factory=set)
    completed: set[str] = field(default_factory=set)
    failed: set[str] = field(default_factory=set)
    callback: Callable[["DsperseRun"], None] | None = None
    environment: dict = field(default_factory=dict)
    start_time: float = 0.0
    circuit_name: str = ""

    @property
    def is_complete(self) -> bool:
        return len(self.pending) == 0

    @property
    def all_successful(self) -> bool:
        return self.is_complete and len(self.failed) == 0


class DSperseManager:
    def __init__(
        self,
        event_client: "DsperseEventClient | None" = None,
        lazy: bool = False,
        incremental_mode: bool = True,
    ):
        """
        Initialize DSperseManager.

        Args:
            event_client: Optional event client for telemetry
            lazy: If True, extract slices on-demand
            incremental_mode: If True, use incremental execution where miners
                             compute outputs
        """
        self.circuits: list[Circuit] = [
            circuit
            for circuit in circuit_store.circuits.values()
            if circuit.metadata.type == CircuitType.DSPERSE_PROOF_GENERATION
        ]
        self.runs: dict[str, DsperseRun] = {}
        self.event_client = event_client
        self.lazy = lazy
        self.incremental_mode = incremental_mode
        self._incremental_runner: IncrementalRunner | None = None
        self._incremental_runs: set[str] = set()
        self._incremental_runs_lock = threading.Lock()
        if incremental_mode:
            self._incremental_runner = IncrementalRunner(
                on_run_complete=self._on_incremental_run_complete,
                on_jstprove_range_fallback=self._on_jstprove_range_fallback,
            )
        self._purge_old_runs()

    @staticmethod
    def _purge_old_runs():
        run_dir = Path(cli_parser.config.dsperse_run_dir)
        if not run_dir.exists():
            return
        entries = list(run_dir.iterdir())
        if not entries:
            return
        logging.info(f"Purging {len(entries)} old dsperse runs from {run_dir}")
        for entry in entries:
            try:
                if entry.is_dir():
                    shutil.rmtree(entry)
                else:
                    entry.unlink()
            except Exception as e:
                logging.warning(f"Failed to remove {entry}: {e}")

    def _schedule_async(self, coro):
        try:
            loop = asyncio.get_running_loop()
            loop.create_task(coro)
        except RuntimeError:
            logging.warning(f"No running event loop, dropping coroutine: {coro!r}")
            coro.close()

    def _get_circuit_by_id(self, circuit_id: str) -> Circuit:
        circuit = next((c for c in self.circuits if c.id == circuit_id), None)
        if circuit is None:
            circuit = circuit_store.ensure_circuit(circuit_id)
            if circuit.metadata.type != CircuitType.DSPERSE_PROOF_GENERATION:
                raise ValueError(
                    f"Circuit {circuit_id} is not a DSperse circuit (type: {circuit.metadata.type})"
                )
            self.circuits.append(circuit)
        return circuit

    def start_run(
        self,
        circuit: Circuit,
        inputs: dict | None = None,
        callback: Callable[[DsperseRun], None] | None = None,
    ) -> tuple[str, list[DSliceQueuedProofRequest]]:
        run_uid = datetime.now().strftime("%Y%m%d%H%M%S%f")
        start_time = time.perf_counter()
        logging.info(
            f"Starting DSperse run for circuit {circuit.metadata.name}. Run UID: {run_uid}"
        )

        environment = capture_environment()

        run_dir = Path(tempfile.mkdtemp(prefix=f"dsperse_run_{run_uid}_"))

        input_json_path = run_dir / "input.json"
        if inputs is None:
            inputs = circuit.input_handler(RequestType.BENCHMARK).generate()
        with open(input_json_path, "w") as f:
            json.dump(inputs, f)

        runner = Runner(
            run_dir=run_dir, threads=os.cpu_count() or 4, batch=True, lazy=self.lazy
        )
        results = runner.run(
            input_json_path=input_json_path,
            slice_path=str(circuit.paths.base_path),
        )
        actual_run_dir = runner.last_run_dir
        logging.debug(f"DSperse run completed. Results at {actual_run_dir}")

        slice_results: dict[str, ExecutionInfo] = results.get("slice_results", {})
        if not all(info.success for info in slice_results.values()):
            failed = [k for k, v in slice_results.items() if not v.success]
            logging.error(f"DSperse witness generation failed for slices: {failed}")
            shutil.rmtree(run_dir, ignore_errors=True)
            return run_uid, []

        slice_data_list = self._extract_dslice_data(
            slice_results,
            actual_run_dir,
            circuit.id,
            runner.run_metadata,
            tensor_cache=runner.tensor_cache,
        )

        dsperse_run = DsperseRun(
            run_uid=run_uid,
            circuit_id=circuit.id,
            run_dir=run_dir,
            slices={s.slice_num: s for s in slice_data_list},
            pending={s.slice_num for s in slice_data_list},
            callback=callback,
            environment=environment,
            start_time=start_time,
            circuit_name=circuit.metadata.name,
        )
        self.runs[run_uid] = dsperse_run

        requests = []
        for slice_data in slice_data_list:
            with open(slice_data.input_file, "r") as f:
                slice_inputs = json.load(f)
            with open(slice_data.output_file, "r") as f:
                slice_outputs = json.load(f)

            requests.append(
                DSliceQueuedProofRequest(
                    circuit=circuit,
                    inputs=slice_inputs,
                    outputs=slice_outputs,
                    slice_num=slice_data.slice_num,
                    run_uid=run_uid,
                    proof_system=slice_data.proof_system,
                )
            )

        logging.info(f"Generated {len(requests)} DSlice requests for run {run_uid}")

        if self.event_client:
            self._schedule_async(
                self.event_client.emit_run_started(
                    run_uid=run_uid,
                    circuit_id=circuit.id,
                    circuit_name=circuit.metadata.name,
                    total_slices=len(slice_data_list),
                    environment=environment,
                )
            )
            for slice_data in slice_data_list:
                if slice_data.timing:
                    self._schedule_async(
                        self.event_client.emit_witness_complete(
                            run_uid=run_uid,
                            slice_num=slice_data.slice_num,
                            witness_time_sec=slice_data.timing.witness_time_sec,
                            memory_peak_mb=slice_data.timing.memory_peak_mb,
                        )
                    )

        return run_uid, requests

    def generate_dslice_requests(self) -> list[DSliceQueuedProofRequest]:
        """Generate DSlice requests using standard (pre-computed) or incremental mode."""
        if not self.circuits:
            return []

        if self.incremental_mode:
            return self.generate_incremental_request()

        circuit = random.choice(self.circuits)
        _, requests = self.start_run(circuit)
        return requests

    def start_incremental_run(
        self,
        circuit: Circuit,
        inputs: dict | None = None,
    ) -> str:
        """
        Start an incremental run where miners compute outputs.

        Args:
            circuit: The circuit to execute
            inputs: Model inputs (generated if not provided)

        Returns:
            Run UID
        """
        if not self._incremental_runner:
            self._incremental_runner = IncrementalRunner(
                on_run_complete=self._on_incremental_run_complete,
                on_jstprove_range_fallback=self._on_jstprove_range_fallback,
            )

        run_uid = self._incremental_runner.start_run(circuit, inputs)
        with self._incremental_runs_lock:
            self._incremental_runs.add(run_uid)

        if self.event_client:
            status = self._incremental_runner.get_run_status(run_uid)
            self._schedule_async(
                self.event_client.emit_run_started(
                    run_uid=run_uid,
                    circuit_id=circuit.id,
                    circuit_name=circuit.metadata.name,
                    total_slices=status["total_slices"] if status else 0,
                    environment=capture_environment(),
                )
            )

        return run_uid

    def get_next_incremental_work(self, run_uid: str) -> list[DSliceQueuedProofRequest]:
        """
        Get the next work items for an incremental run.

        Args:
            run_uid: The run identifier

        Returns:
            List of DSliceQueuedProofRequest. Empty list means ONNX ran locally (call again).
            None returned as empty list means waiting or complete.
        """
        if not self._incremental_runner:
            return []

        work_items = self._incremental_runner.get_next_work(run_uid)
        if work_items is None:
            return []
        if len(work_items) == 0:
            return self.get_next_incremental_work(run_uid)

        queued_requests = [
            self._incremental_runner.create_queued_request(item) for item in work_items
        ]
        logging.info(
            f"Converted {len(work_items)} work items to {len(queued_requests)} queued requests"
        )
        return queued_requests

    def generate_incremental_request(self) -> list[DSliceQueuedProofRequest]:
        """
        Generate incremental request(s).

        Returns work items for the next slice (1 for non-tiled, N for tiled).
        If there's an active incremental run, returns the next work.
        Otherwise starts a new run.

        Returns:
            List of DSliceQueuedProofRequest
        """
        if not self._incremental_runner:
            self._incremental_runner = IncrementalRunner(
                on_run_complete=self._on_incremental_run_complete,
                on_jstprove_range_fallback=self._on_jstprove_range_fallback,
            )

        with self._incremental_runs_lock:
            incremental_runs_snapshot = list(self._incremental_runs)

        for run_uid in incremental_runs_snapshot:
            if not self._incremental_runner.is_complete(run_uid):
                requests = self.get_next_incremental_work(run_uid)
                if requests:
                    logging.info(
                        f"Generating {len(requests)} work items for run {run_uid}"
                    )
                    return requests

        if not self.circuits:
            return []
        circuit = random.choice(self.circuits)
        run_uid = self.start_incremental_run(circuit)

        requests = self.get_next_incremental_work(run_uid)
        if requests:
            logging.info(f"Generating {len(requests)} work items for new run {run_uid}")
            return requests

        return []

    def on_incremental_slice_result(
        self,
        run_uid: str,
        slice_num: str,
        success: bool,
        computed_outputs: dict | None = None,
        proof: str | None = None,
        response_time_sec: float = 0.0,
        verification_time_sec: float = 0.0,
    ) -> tuple[bool, DSliceQueuedProofRequest | None]:
        """
        Handle result for an incremental slice.

        Args:
            run_uid: The run identifier
            slice_num: The slice number
            success: Whether the slice succeeded
            computed_outputs: Outputs computed by the miner
            proof: Proof generated by the miner
            response_time_sec: Response time
            verification_time_sec: Verification time

        Returns:
            Tuple of (is_run_complete, next_slice_request)
        """
        if not self._incremental_runner:
            return True, None

        task_id = (
            f"slice_{slice_num}" if not slice_num.startswith("slice_") else slice_num
        )

        slice_complete = self._incremental_runner.apply_result(
            run_uid=run_uid,
            task_id=task_id,
            success=success,
            output=computed_outputs,
            error=None if success else "Slice execution failed",
        )

        if self.event_client:
            if success:
                self._schedule_async(
                    self.event_client.emit_verification_complete(
                        run_uid=run_uid,
                        slice_num=slice_num,
                        verification_time_sec=verification_time_sec,
                        success=True,
                    )
                )
            else:
                self._schedule_async(
                    self.event_client.emit_slice_failed(
                        run_uid=run_uid,
                        slice_num=slice_num,
                    )
                )

        is_complete = self._incremental_runner.is_complete(run_uid)
        if is_complete:
            return True, None

        if not slice_complete:
            return False, None

        next_requests = self.get_next_incremental_work(run_uid)
        return False, next_requests[0] if next_requests else None

    def on_incremental_tile_result(
        self,
        run_uid: str,
        task_id: str,
        slice_id: str,
        tile_idx: int,
        success: bool,
        computed_outputs: dict | None = None,
        proof: str | None = None,
        witness: str | None = None,
        response_time_sec: float = 0.0,
        verification_time_sec: float = 0.0,
    ) -> tuple[bool, list[DSliceQueuedProofRequest]]:
        """
        Handle result for an incremental tile.

        Args:
            run_uid: The run identifier
            task_id: The tile task identifier
            slice_id: The parent slice identifier (e.g., "slice_0")
            tile_idx: The tile index
            success: Whether the tile succeeded
            computed_outputs: Outputs computed by the miner
            proof: Proof generated by the miner (hex string)
            witness: Witness generated by the miner (hex string)
            response_time_sec: Response time
            verification_time_sec: Verification time

        Returns:
            Tuple of (is_run_complete, next_requests)
        """
        if not self._incremental_runner:
            return True, []

        slice_complete = self._incremental_runner.apply_result(
            run_uid=run_uid,
            task_id=task_id,
            success=success,
            output=computed_outputs,
            error=None if success else "Tile execution failed",
        )

        if self.event_client:
            slice_num = f"{slice_id.removeprefix('slice_')}_tile_{tile_idx}"
            if success:
                self._schedule_async(
                    self.event_client.emit_verification_complete(
                        run_uid=run_uid,
                        slice_num=slice_num,
                        verification_time_sec=verification_time_sec,
                        success=True,
                    )
                )
            else:
                self._schedule_async(
                    self.event_client.emit_slice_failed(
                        run_uid=run_uid,
                        slice_num=slice_num,
                    )
                )

        is_complete = self._incremental_runner.is_complete(run_uid)
        if is_complete:
            return True, []

        if not slice_complete:
            return False, []

        return False, self.get_next_incremental_work(run_uid)

    def has_work_in_flight(self) -> bool:
        """Check if any run has work currently being processed by miners."""
        if not self._incremental_runner:
            return False
        with self._incremental_runs_lock:
            for run_uid in self._incremental_runs:
                if self._incremental_runner.has_pending_work(run_uid):
                    return True
        return False

    def _on_jstprove_range_fallback(
        self, run_uid: str, slice_id: str, overflow_info: dict
    ) -> None:
        """Callback when a JSTprove slice falls back to ONNX due to range overflow."""
        logging.warning(
            f"JSTprove range overflow in run {run_uid} slice {slice_id}: "
            f"{overflow_info.get('overflow_count')}/{overflow_info.get('total_elements')} "
            f"values exceed {overflow_info.get('n_bits')}-bit range"
        )
        if self.event_client:
            self._schedule_async(
                self.event_client.emit_jstprove_range_overflow(
                    run_uid=run_uid,
                    slice_num=slice_id,
                    overflow_info=overflow_info,
                )
            )

    def _on_incremental_run_complete(self, run_uid: str, success: bool) -> None:
        """Callback when an incremental run completes."""
        logging.info(f"Incremental run {run_uid} completed, success={success}")

        with self._incremental_runs_lock:
            self._incremental_runs.discard(run_uid)

            if self.event_client:
                status = (
                    self._incremental_runner.get_run_status(run_uid)
                    if self._incremental_runner
                    else None
                )
                self._schedule_async(
                    self.event_client.emit_run_complete(
                        run_uid=run_uid,
                        all_successful=success,
                        total_run_time_sec=(
                            status.get("elapsed_time", 0) if status else 0
                        ),
                    )
                )

            if self._incremental_runner:
                self._incremental_runner.cleanup_run(run_uid)

    def is_incremental_run(self, run_uid: str) -> bool:
        """Check if a run is an incremental run."""
        with self._incremental_runs_lock:
            return run_uid in self._incremental_runs

    def on_slice_result(
        self,
        run_uid: str,
        slice_num: str,
        success: bool,
        response_time_sec: float = 0.0,
        verification_time_sec: float = 0.0,
    ) -> bool:
        if run_uid not in self.runs:
            logging.warning(f"on_slice_result: Run {run_uid} not found")
            return False

        run = self.runs[run_uid]
        if slice_num not in run.pending:
            logging.warning(
                f"on_slice_result: Slice {slice_num} not pending in run {run_uid}"
            )
            return False

        run.pending.discard(slice_num)
        if success:
            run.completed.add(slice_num)
        else:
            run.failed.add(slice_num)

        if slice_num in run.slices:
            run.slices[slice_num].success = success
            if run.slices[slice_num].timing:
                run.slices[slice_num].timing.response_time_sec = response_time_sec
                run.slices[slice_num].timing.verification_time_sec = (
                    verification_time_sec
                )
                run.slices[slice_num].timing.success = success

        if self.event_client:
            if success:
                self._schedule_async(
                    self.event_client.emit_verification_complete(
                        run_uid=run_uid,
                        slice_num=slice_num,
                        verification_time_sec=verification_time_sec,
                        success=True,
                    )
                )
            else:
                self._schedule_async(
                    self.event_client.emit_slice_failed(
                        run_uid=run_uid,
                        slice_num=slice_num,
                    )
                )

        if run.is_complete:
            logging.info(
                f"Run {run_uid} complete. "
                f"Completed: {len(run.completed)}, Failed: {len(run.failed)}"
            )
            total_run_time = (
                time.perf_counter() - run.start_time if run.start_time else None
            )
            if self.event_client:
                self._schedule_async(
                    self.event_client.emit_run_complete(
                        run_uid=run_uid,
                        all_successful=run.all_successful,
                        total_run_time_sec=total_run_time,
                    )
                )
            try:
                loop = asyncio.get_running_loop()
                loop.run_in_executor(None, self._submit_metrics, run)
            except RuntimeError:
                threading.Thread(
                    target=self._submit_metrics, args=(run,), daemon=True
                ).start()
            if run.callback:
                try:
                    run.callback(run)
                except Exception as e:
                    logging.error(f"Run callback failed: {e}")
            self.cleanup_run(run_uid)

        return run.is_complete

    def _submit_metrics(self, run: DsperseRun):
        """Submit dsperse run metrics to sn2-api."""
        try:
            import httpx

            total_run_time = (
                time.perf_counter() - run.start_time if run.start_time else 0.0
            )

            total_witness_time = 0.0
            total_response_time = 0.0
            total_verification_time = 0.0
            circuit_slices = 0
            onnx_slices = 0

            slice_metrics = []
            for slice_data in run.slices.values():
                if slice_data.timing:
                    timing = slice_data.timing
                    total_witness_time += timing.witness_time_sec
                    total_response_time += timing.response_time_sec
                    total_verification_time += timing.verification_time_sec
                    circuit_slices += 1

                    slice_metrics.append(
                        {
                            "slice_num": timing.slice_num,
                            "proof_system": timing.proof_system,
                            "backend_used": timing.backend_used,
                            "witness_time_sec": timing.witness_time_sec,
                            "response_time_sec": timing.response_time_sec,
                            "verification_time_sec": timing.verification_time_sec,
                            "is_tiled": timing.is_tiled,
                            "tile_count": timing.tile_count,
                            "memory_peak_mb": timing.memory_peak_mb,
                            "success": timing.success,
                            "error": timing.error,
                        }
                    )
                else:
                    onnx_slices += 1

            payload = {
                "run_uid": run.run_uid,
                "validator_key": self._get_validator_hotkey(),
                "circuit_id": run.circuit_id,
                "circuit_name": run.circuit_name,
                "total_slices": len(run.slices),
                "circuit_slices": circuit_slices,
                "onnx_slices": onnx_slices,
                "total_witness_time_sec": total_witness_time,
                "total_response_time_sec": total_response_time,
                "total_verification_time_sec": total_verification_time,
                "total_run_time_sec": total_run_time,
                "all_successful": run.all_successful,
                "failed_slice_count": len(run.failed),
                "environment": run.environment,
                "slices": slice_metrics,
            }

            api_url = getattr(cli_parser.config, "sn2_api_url", None)
            if not api_url:
                api_url = "https://sn2-api.inferencelabs.com"

            hotkey = self._get_validator_hotkey()
            body = json.dumps(payload, sort_keys=True, separators=(",", ":"))
            signature = self._sign_request(body)

            if hotkey == "unknown" or not signature:
                logging.warning(
                    f"Skipping metrics submission for run {run.run_uid}: "
                    f"invalid hotkey or signature"
                )
                return

            with httpx.Client(timeout=30.0) as client:
                response = client.post(
                    f"{api_url}/statistics/dsperse/log/",
                    content=body,
                    headers={
                        "Content-Type": "application/json",
                        "X-Signature": signature,
                        "X-Hotkey": hotkey,
                    },
                )
                if response.status_code == 200:
                    logging.debug(f"Metrics submitted for run {run.run_uid}")
                else:
                    logging.warning(
                        f"Failed to submit metrics for run {run.run_uid}: "
                        f"{response.status_code} - {response.text}"
                    )
        except Exception as e:
            logging.warning(f"Failed to submit dsperse metrics: {e}")

    def _get_validator_hotkey(self) -> str:
        try:
            return cli_parser.config.wallet.hotkey.ss58_address
        except Exception:
            return "unknown"

    def _sign_request(self, body: str) -> str:
        try:
            import hashlib

            wallet = cli_parser.config.wallet
            message = hashlib.sha256(body.encode()).hexdigest()
            signature = wallet.hotkey.sign(message.encode())
            return signature.hex()
        except Exception:
            return ""

    def get_run_status(self, run_uid: str) -> dict | None:
        if run_uid not in self.runs:
            return None
        run = self.runs[run_uid]
        total = len(run.slices)
        return {
            "run_uid": run_uid,
            "circuit_id": run.circuit_id,
            "total_slices": total,
            "pending": len(run.pending),
            "completed": len(run.completed),
            "failed": len(run.failed),
            "is_complete": run.is_complete,
            "all_successful": run.all_successful,
            "progress_percent": (
                (len(run.completed) + len(run.failed)) / total * 100 if total > 0 else 0
            ),
        }

    @staticmethod
    def _extract_dslice_data(
        slice_results: dict[str, ExecutionInfo],
        run_dir: Path,
        circuit_id: str,
        run_metadata: RunMetadata | None = None,
        tensor_cache: dict | None = None,
    ) -> list[DSliceData]:
        dslice_data_list = []

        for slice_num, exec_info in slice_results.items():
            method = str(exec_info.method)
            if method.startswith("onnx"):
                continue

            if run_metadata:
                node = run_metadata.execution_chain.nodes.get(slice_num)
                if node and not node.use_circuit:
                    continue

            base_slice_num = slice_num.split("_")[-1]
            slice_run_dir = run_dir / slice_num

            if exec_info.method == ExecutionMethod.TILED:
                circuit_tiles = [
                    (idx, t)
                    for idx, t in enumerate(exec_info.tiles)
                    if not str(t.method).startswith("onnx")
                ]
                if not circuit_tiles:
                    continue
                logging.info(
                    f"Expanding tiled slice {slice_num} into {len(circuit_tiles)} tile requests"
                )
                for tile_idx, tile_result in circuit_tiles:
                    tile_run_dir = slice_run_dir / f"tile_{tile_idx}"
                    tile_input = tile_run_dir / "input.json"
                    tile_output = tile_run_dir / "output.json"

                    if not tile_input.exists() or not tile_output.exists():
                        if tensor_cache is None:
                            raise ValueError(
                                f"Tile {tile_idx} of slice {slice_num} missing input/output files"
                            )
                        DSperseManager._write_tile_files_from_cache(
                            tensor_cache,
                            base_slice_num,
                            tile_idx,
                            tile_input,
                            tile_output,
                        )

                    proof_system = DSperseManager._method_to_proof_system(
                        tile_result.method
                    )
                    if proof_system is None:
                        logging.error(
                            f"Skipping tile {tile_idx} of slice {slice_num}: unknown method"
                        )
                        continue

                    timing = SliceTimingData(
                        slice_num=f"{base_slice_num}_tile_{tile_idx}",
                        proof_system=(
                            str(tile_result.method) if tile_result.method else None
                        ),
                        backend_used=(
                            str(tile_result.method) if tile_result.method else None
                        ),
                        witness_time_sec=tile_result.time_sec,
                        is_tiled=True,
                        tile_count=len(circuit_tiles),
                        success=tile_result.success,
                        error=tile_result.error,
                    )

                    dslice_data_list.append(
                        DSliceData(
                            slice_num=f"{base_slice_num}_tile_{tile_idx}",
                            input_file=tile_input,
                            output_file=tile_output,
                            witness_file=tile_run_dir / "output_witness.bin",
                            circuit_id=circuit_id,
                            proof_system=proof_system,
                            timing=timing,
                        )
                    )
            else:
                slice_input = slice_run_dir / "input.json"
                slice_output = slice_run_dir / "output.json"

                if not slice_input.exists() or not slice_output.exists():
                    logging.warning(f"Slice {slice_num} missing input/output files")
                    continue

                proof_system = DSperseManager._method_to_proof_system(method)
                if proof_system is None:
                    logging.error(
                        f"Skipping slice {slice_num}: unknown method '{method}'"
                    )
                    continue

                timing = SliceTimingData(
                    slice_num=base_slice_num,
                    proof_system=method,
                    backend_used=method,
                    witness_time_sec=getattr(exec_info, "time_sec", 0.0),
                    memory_peak_mb=getattr(exec_info, "memory_peak_mb", 0.0),
                    is_tiled=False,
                    success=exec_info.success,
                    error=exec_info.error,
                )

                dslice_data_list.append(
                    DSliceData(
                        slice_num=base_slice_num,
                        input_file=slice_input,
                        output_file=slice_output,
                        witness_file=slice_run_dir / "output_witness.bin",
                        circuit_id=circuit_id,
                        proof_system=proof_system,
                        timing=timing,
                    )
                )

        logging.info(f"Generated {len(dslice_data_list)} DSlice requests")
        return dslice_data_list

    @staticmethod
    def _write_tile_files_from_cache(
        tensor_cache: dict,
        slice_idx: str,
        tile_idx: int,
        input_path: Path,
        output_path: Path,
    ) -> None:
        input_key = f"tile_{slice_idx}_{tile_idx}_in"
        output_key = f"tile_{slice_idx}_{tile_idx}_out"

        input_tensor = tensor_cache.get(input_key)
        output_tensor = tensor_cache.get(output_key)

        if input_tensor is None or output_tensor is None:
            raise ValueError(
                f"Tile {tile_idx} of slice_{slice_idx} missing from tensor_cache "
                f"(input={input_key in tensor_cache}, output={output_key in tensor_cache})"
            )

        input_path.parent.mkdir(parents=True, exist_ok=True)

        input_list = (
            input_tensor.tolist() if hasattr(input_tensor, "tolist") else input_tensor
        )
        output_list = (
            output_tensor.tolist()
            if hasattr(output_tensor, "tolist")
            else output_tensor
        )

        with open(input_path, "w") as f:
            json.dump({"input_data": input_list}, f)
        with open(output_path, "w") as f:
            json.dump({"output_data": output_list}, f)

        logging.debug(
            f"Materialized tile files from cache: {input_path}, {output_path}"
        )

    @staticmethod
    def _method_to_proof_system(method: str | None) -> ProofSystem | None:
        if not method:
            logging.error("No proof method specified")
            return None
        method_lower = str(method).lower()
        if "ezkl" in method_lower:
            return ProofSystem.EZKL
        if "jstprove" in method_lower or "jst" in method_lower:
            return ProofSystem.JSTPROVE
        logging.error(f"Unknown proof method '{method}'")
        return None

    def prove_slice(
        self,
        circuit_id: str,
        slice_num: str,
        inputs: dict,
        outputs: dict | None,
        proof_system: ProofSystem,
    ) -> dict:
        """
        Generate proof for a slice.

        In standard mode, both inputs and outputs are provided.
        In incremental mode (outputs=None), inference is run to compute outputs,
        which are returned in the result.

        Args:
            circuit_id: The circuit identifier
            slice_num: Slice number (may include tile suffix like "0_tile_1")
            inputs: Input tensor data
            outputs: Expected output data (None for incremental mode)
            proof_system: Proof system to use (JSTPROVE or EZKL)

        Returns:
            Dict with success, proof, proof_generation_time, and witness (for incremental)
        """
        incremental_mode = outputs is None
        circuit = self._get_circuit_by_id(circuit_id)
        base_slice_num, tile_idx = self._parse_slice_num(slice_num)
        model_dir = Path(circuit.paths.base_path) / f"slice_{base_slice_num}"
        result = {
            "circuit_id": circuit_id,
            "slice_num": slice_num,
            "success": False,
            "proof_generation_time": None,
            "proof": None,
        }

        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp_path = Path(tmp_dir)
            input_file = tmp_path / "input.json"
            output_file = tmp_path / "output.json"

            with open(input_file, "w") as f:
                json.dump(inputs, f)

            if tile_idx is not None:
                return self._prove_tile(
                    model_dir,
                    base_slice_num,
                    tile_idx,
                    input_file,
                    output_file,
                    tmp_path,
                    proof_system,
                    result,
                    incremental_mode=incremental_mode,
                )

            slice_copy = tmp_path / "slices"
            shutil.copytree(model_dir, slice_copy)
            runner = Runner(run_dir=tmp_path, threads=os.cpu_count() or 4, batch=True)
            runner.run(input_json_path=input_file, slice_path=str(slice_copy))
            run_dir = runner.last_run_dir
            logging.info(f"Runner completed for slice_{slice_num}, run_dir: {run_dir}")

            if runner.run_metadata:
                metadata_path = run_dir / "metadata.json"
                with open(metadata_path, "w") as f:
                    json.dump(runner.run_metadata.to_dict(), f)

            prove_start = time.time()

            if proof_system == ProofSystem.JSTPROVE:
                jst_model_path = self._find_jstprove_circuit(
                    model_dir, base_slice_num, is_tiled=False
                )
                if jst_model_path is None or not jst_model_path.exists():
                    logging.error(f"JSTprove circuit not found for slice {slice_num}")
                    return result
                success, proof_data, witness_data, _ = self._jstprove_witness_and_prove(
                    jst_model_path,
                    input_file,
                    output_file,
                    tmp_path,
                    f"slice {slice_num}",
                )
                if incremental_mode and witness_data is not None:
                    result["witness"] = witness_data
                if not success and proof_data is None:
                    return result

            elif proof_system == ProofSystem.EZKL:
                slice_id = f"slice_{base_slice_num}"
                slice_meta = (
                    runner.run_metadata.get_slice(slice_id)
                    if runner.run_metadata
                    else None
                )
                if not slice_meta:
                    logging.error(
                        f"No run metadata for {slice_id}, cannot prove with EZKL"
                    )
                    return result

                ezkl_circuit_path = (
                    slice_meta.ezkl_circuit_path or slice_meta.circuit_path
                )
                ezkl_pk_path = slice_meta.ezkl_pk_path or slice_meta.pk_path
                ezkl_settings_path = (
                    slice_meta.ezkl_settings_path or slice_meta.settings_path
                )

                if not ezkl_circuit_path or not ezkl_pk_path or not ezkl_settings_path:
                    logging.error(
                        f"Missing EZKL paths for {slice_id}: "
                        f"circuit={ezkl_circuit_path}, pk={ezkl_pk_path}, settings={ezkl_settings_path}"
                    )
                    return result

                ezkl_circuit = (
                    Path(ezkl_circuit_path)
                    if Path(ezkl_circuit_path).is_absolute()
                    else model_dir / ezkl_circuit_path
                )
                ezkl_pk = (
                    Path(ezkl_pk_path)
                    if Path(ezkl_pk_path).is_absolute()
                    else model_dir / ezkl_pk_path
                )
                ezkl_settings = (
                    Path(ezkl_settings_path)
                    if Path(ezkl_settings_path).is_absolute()
                    else model_dir / ezkl_settings_path
                )
                witness_path = run_dir / slice_id / "output.json"
                proof_path = tmp_path / "proof.json"

                ezkl_runner = EZKL()
                success, proof_file = ezkl_runner.prove(
                    witness_path=str(witness_path),
                    model_path=str(ezkl_circuit),
                    proof_path=str(proof_path),
                    pk_path=str(ezkl_pk),
                    settings_path=str(ezkl_settings),
                )

                proof_data = None
                if success and proof_path.exists():
                    with open(proof_path, "r") as pf:
                        proof_data = json.load(pf)
            else:
                logging.error(f"Unsupported proof system: {proof_system}")
                return result

            proof_generation_time = time.time() - prove_start
            result["success"] = success
            result["proof_generation_time"] = proof_generation_time
            result["proof"] = proof_data
            return result

    @staticmethod
    def _jstprove_witness_and_prove(
        circuit_path: Path,
        input_file: Path,
        output_file: Path,
        tmp_path: Path,
        label: str,
    ) -> tuple[bool, str | None, str | None, None]:
        """
        Generate witness and proof using JSTprove.

        Args:
            circuit_path: Path to the circuit file
            input_file: Path to input JSON
            output_file: Path to write output JSON
            tmp_path: Temporary directory for artifacts
            label: Label for logging

        Returns:
            Tuple of (success, proof_hex, witness_hex, None)
        """
        jstprover = JSTprove()
        success, res = jstprover.generate_witness(
            input_file=input_file,
            model_path=circuit_path,
            output_file=output_file,
        )
        if not success:
            logging.error(f"Failed to generate witness for {label}: {res}")
            return False, None, None, None

        witness_path = tmp_path / "output_witness.bin"
        proof_path = tmp_path / "proof.bin"
        success, _ = jstprover.prove(
            witness_path=str(witness_path),
            circuit_path=str(circuit_path),
            proof_path=str(proof_path),
        )

        proof_data = None
        witness_data = None
        if success and proof_path.exists():
            with open(proof_path, "rb") as pf:
                proof_data = pf.read().hex()
        if success and witness_path.exists():
            with open(witness_path, "rb") as wf:
                witness_data = wf.read().hex()
        return success, proof_data, witness_data, None

    def _prove_tile(
        self,
        model_dir: Path,
        base_slice_num: str,
        tile_idx: int,
        input_file: Path,
        output_file: Path,
        tmp_path: Path,
        proof_system: ProofSystem,
        result: dict,
        incremental_mode: bool = False,
    ) -> dict:
        prove_start = time.time()
        slice_num = f"{base_slice_num}_tile_{tile_idx}"

        if proof_system == ProofSystem.JSTPROVE:
            jst_tile_circuit = self._find_jstprove_circuit(
                model_dir, base_slice_num, is_tiled=True
            )
            if jst_tile_circuit is None or not jst_tile_circuit.exists():
                logging.error(f"Tile JSTprove circuit not found for slice {slice_num}")
                return result

            success, proof_data, witness_data, _ = self._jstprove_witness_and_prove(
                jst_tile_circuit,
                input_file,
                output_file,
                tmp_path,
                f"tile {slice_num}",
            )
            if incremental_mode and witness_data is not None:
                result["witness"] = witness_data
        else:
            logging.error(f"Proof system {proof_system} not supported for tiles")
            return result

        proof_generation_time = time.time() - prove_start
        result["success"] = success
        result["proof_generation_time"] = proof_generation_time
        result["proof"] = proof_data
        return result

    def _parse_slice_num(self, slice_num: str) -> tuple[str, int | None]:
        if "_tile_" in slice_num:
            parts = slice_num.split("_tile_")
            return parts[0], int(parts[1])
        return slice_num, None

    def _find_jstprove_circuit(
        self, model_dir: Path, base_slice_num: str, is_tiled: bool
    ) -> Path | None:
        """Find JSTprove circuit path, checking both current and legacy locations."""
        if is_tiled:
            paths = [
                model_dir / "jstprove" / "tiles" / "tile_circuit.txt",
                model_dir / "payload" / "jstprove" / "tiles" / "tile_circuit.txt",
            ]
        else:
            slice_id = f"slice_{base_slice_num}"
            paths = [
                model_dir / "jstprove" / f"{slice_id}_circuit.txt",
                model_dir / "payload" / "jstprove" / f"{slice_id}_circuit.txt",
            ]
        for p in paths:
            if p.exists():
                return p
        return None

    def verify_slice_proof(
        self, run_uid: str, slice_num: str, proof: dict | str, proof_system: ProofSystem
    ) -> bool:
        if run_uid not in self.runs:
            raise ValueError(f"Run UID {run_uid} not found.")

        run = self.runs[run_uid]
        slice_data = run.slices.get(slice_num)
        if slice_data is None:
            raise ValueError(f"Slice {slice_num} not found in run {run_uid}.")
        if slice_data.proof_system != proof_system:
            raise ValueError(
                f"Proof system mismatch for slice {slice_num}. "
                f"Expected {slice_data.proof_system}, got {proof_system}."
            )

        circuit = self._get_circuit_by_id(slice_data.circuit_id)
        base_slice_num, tile_idx = self._parse_slice_num(slice_num)

        proof_file_path = slice_data.input_file.parent / "proof.json"
        if proof_system == ProofSystem.JSTPROVE:
            if not isinstance(proof, str):
                logging.error(f"JSTPROVE proof must be a hex string, got {type(proof)}")
                return False
            try:
                proof_bytes = bytes.fromhex(proof)
            except ValueError as e:
                logging.error(f"Invalid hex in JSTPROVE proof: {e}")
                return False
            with open(proof_file_path, "wb") as proof_file:
                proof_file.write(proof_bytes)
        else:
            with open(proof_file_path, "w") as proof_file:
                json.dump(proof, proof_file)

        slice_data.proof_file = proof_file_path

        if proof_system == ProofSystem.JSTPROVE:
            slice_dir = Path(circuit.paths.base_path) / f"slice_{base_slice_num}"
            circuit_path = self._find_jstprove_circuit(
                slice_dir, base_slice_num, is_tiled=(tile_idx is not None)
            )
            if circuit_path is None or not circuit_path.exists():
                logging.error(f"JSTprove circuit not found for slice {slice_num}")
                return False
            witness_path = slice_data.witness_file or (
                slice_data.input_file.parent / "output_witness.bin"
            )

            jstprove = JSTprove()
            success = jstprove.verify(
                proof_path=proof_file_path,
                circuit_path=circuit_path,
                input_path=slice_data.input_file,
                output_path=slice_data.output_file,
                witness_path=witness_path,
            )
        else:
            verifier = Verifier()
            run_path = slice_data.input_file.parent.parent
            result = verifier.verify(
                run_path=run_path,
                model_path=Path(circuit.paths.base_path) / f"slice_{base_slice_num}",
                backend=proof_system.value.lower() if proof_system else None,
            )
            _, verification_execution = self._parse_dsperse_result(
                result, "verification"
            )
            success = verification_execution.get("success", False)

        slice_data.success = success
        return success

    def verify_incremental_slice_with_witness(
        self,
        circuit_id: str,
        slice_num: str,
        original_inputs: dict,
        witness_hex: str,
        proof_hex: str,
        proof_system: ProofSystem,
    ) -> tuple[bool, Optional[torch.Tensor]]:
        """
        Verify a proof using the witness, extracting and validating inputs/outputs.

        Delegates to dsperse's JSTprove.verify_with_io_extraction for trustless
        verification where inputs/outputs are extracted from the cryptographically
        bound witness rather than trusting miner-provided values.

        Args:
            circuit_id: The circuit identifier
            slice_num: The slice number
            original_inputs: The inputs that were sent to the miner
            witness_hex: Hex-encoded witness from miner
            proof_hex: Hex-encoded proof from miner
            proof_system: The proof system used

        Returns:
            Tuple of (success, output_tensor)
        """
        if proof_system != ProofSystem.JSTPROVE:
            logging.error("Trustless verification only implemented for JSTPROVE")
            return False, None

        circuit = self._get_circuit_by_id(circuit_id)
        base_slice_num, tile_idx = self._parse_slice_num(slice_num)
        slice_dir = Path(circuit.paths.base_path) / f"slice_{base_slice_num}"
        metadata_path = slice_dir / "metadata.json"

        if not metadata_path.exists():
            logging.error(f"Slice metadata not found: {metadata_path}")
            return False, None

        with open(metadata_path, "r") as f:
            slice_metadata = json.load(f)

        output_shape = None
        if tile_idx is not None:
            slices_list = slice_metadata.get("slices", [])
            tiling = (
                slices_list[0].get("tiling", {})
                if slices_list
                else slice_metadata.get("tiling", {})
            )
            tile_size = tiling.get("tile_size", 0)
            raw_halo = tiling.get("halo", 0)
            if isinstance(raw_halo, int):
                halo = (raw_halo, raw_halo)
            elif isinstance(raw_halo, (list, tuple)):
                if len(raw_halo) == 0:
                    halo = (0, 0)
                elif len(raw_halo) == 1:
                    halo = (int(raw_halo[0]), int(raw_halo[0]))
                else:
                    halo = (int(raw_halo[0]), int(raw_halo[1]))
            else:
                halo = (0, 0)
            c_in = tiling.get("c_in", 1)
            tile_h = tile_size + 2 * halo[0]
            tile_w = tile_size + 2 * halo[1]
            num_inputs = c_in * tile_h * tile_w
            c_out = tiling.get("c_out", 1)
            out_tile = tiling.get("out_tile", [tile_size, tile_size])
            output_shape = [1, c_out, out_tile[0], out_tile[1]]
        else:
            slices_meta = slice_metadata.get("slices", [])
            if not isinstance(slices_meta, list) or not slices_meta:
                logging.error(
                    f"Slice {slice_num}: slices_meta is not a non-empty list: {slices_meta}"
                )
                return False, None
            if not isinstance(slices_meta[0], dict):
                logging.error(
                    f"Slice {slice_num}: slices_meta[0] is not a dict: {slices_meta[0]}"
                )
                return False, None
            tensor_shapes = (
                slices_meta[0].get("shape", {}).get("tensor_shape", {}).get("input", [])
            )
            output_shapes = (
                slices_meta[0]
                .get("shape", {})
                .get("tensor_shape", {})
                .get("output", [])
            )
            if output_shapes:
                output_shape = [d for d in output_shapes[0] if isinstance(d, int)]
            filtered_inputs = (
                slices_meta[0].get("dependencies", {}).get("filtered_inputs", [])
            )
            n_filtered = len(filtered_inputs)
            if tensor_shapes and n_filtered > 0:
                runtime_shapes = tensor_shapes[-n_filtered:]
            else:
                runtime_shapes = tensor_shapes
            num_inputs = 0
            for shape in runtime_shapes:
                int_dims = [d for d in shape if isinstance(d, int)]
                if int_dims:
                    num_inputs += int(np.prod(int_dims))
            logging.debug(
                f"Slice {slice_num}: runtime_shapes={runtime_shapes} -> num_inputs={num_inputs}"
            )

        try:
            witness_bytes = bytes.fromhex(witness_hex)
            proof_bytes = bytes.fromhex(proof_hex)
        except ValueError as e:
            logging.error(f"Invalid hex encoding: {e}")
            return False, None

        circuit_path = self._find_jstprove_circuit(
            slice_dir, base_slice_num, is_tiled=(tile_idx is not None)
        )
        if circuit_path is None or not circuit_path.exists():
            logging.error(f"JSTprove circuit not found for slice {slice_num}")
            return False, None

        flat_inputs = self._flatten_inputs(original_inputs)

        jstprove = JSTprove()
        success, extracted_io = jstprove.verify_with_io_extraction(
            circuit_path=circuit_path,
            witness_bytes=witness_bytes,
            proof_bytes=proof_bytes,
            num_inputs=num_inputs,
            expected_inputs=flat_inputs,
        )

        if not success or extracted_io is None:
            return False, None

        logging.debug("Input verification passed")
        output_tensor = torch.tensor(extracted_io["rescaled_outputs"])
        if output_shape is not None:
            output_tensor = output_tensor.reshape(output_shape)
        return True, output_tensor

    def _flatten_inputs(self, inputs: dict) -> list:
        """Flatten input dict to a list of values.

        Handles:
        - Single input with "input_data" key
        - Single input with arbitrary key
        - Multiple inputs with tensor name keys (concatenated in sorted order)
        """

        def flatten(x):
            if isinstance(x, (list, tuple)):
                result = []
                for item in x:
                    result.extend(flatten(item))
                return result
            return [x]

        if "input_data" in inputs:
            return flatten(inputs["input_data"])

        if len(inputs) == 1:
            return flatten(list(inputs.values())[0])

        result = []
        for key in sorted(inputs.keys()):
            result.extend(flatten(inputs[key]))
        return result

    def cleanup_run(self, run_uid: str):
        if run_uid not in self.runs:
            raise ValueError(f"Run {run_uid} not found.")
        run = self.runs[run_uid]
        if run.run_dir.exists():
            shutil.rmtree(run.run_dir)
        del self.runs[run_uid]
        logging.info(f"Cleaned up run {run_uid}")

    def total_cleanup(self):
        logging.info("Performing total cleanup of all DSperse run data...")
        for run_uid in list(self.runs.keys()):
            try:
                self.cleanup_run(run_uid)
            except Exception as e:
                logging.error(f"Failed to cleanup run {run_uid}: {e}")

    def _parse_dsperse_result(
        self, result: dict, execution_type: str
    ) -> tuple[str | None, dict]:
        execution_results = result.get("execution_chain", {}).get(
            "execution_results", []
        )
        execution_result = execution_results[0] if execution_results else {}
        if not execution_result:
            logging.error("No execution results found in proof generation result.")

        slice_id = execution_result.get("slice_id", None)
        execution = execution_result.get(f"{execution_type}_execution", {})
        if execution is None:
            execution = {}

        return slice_id, execution
