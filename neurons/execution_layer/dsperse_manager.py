import asyncio
import json
import os
import random
import shutil
import tempfile
import threading
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Optional

import bittensor as bt
import torch
from bittensor import logging
from deployment_layer.circuit_store import circuit_store
import time

from dsperse.src.backends.jstprove import JSTprove
from dsperse.src.slice.utils.converter import Converter
from constants import RunSource
from execution_layer.circuit import Circuit, CircuitType, ProofSystem
import numpy as np

from execution_layer.incremental_runner import IncrementalRunner, RunStatus
from utils.system import capture_environment

import cli_parser
from _validator.models.dslice_request import DSliceQueuedProofRequest

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from execution_layer.dsperse_event_client import DsperseEventClient


@dataclass
class _SliceMetric:
    slice_num: str
    response_time_sec: float = 0.0
    verification_time_sec: float = 0.0
    success: bool = True
    is_tile: bool = False
    proof_data: dict | str | None = None
    proof_system: ProofSystem | None = None


@dataclass
class _RunTiming:
    total_response_time_sec: float = 0.0
    total_verification_time_sec: float = 0.0
    slices: list[_SliceMetric] = field(default_factory=list)


_extraction_locks: dict[str, threading.Lock] = {}
_extraction_locks_guard = threading.Lock()


class DSperseManager:
    def __init__(
        self,
        event_client: "DsperseEventClient | None" = None,
    ):
        self.event_client = event_client
        self._loop: asyncio.AbstractEventLoop | None = None
        self._incremental_runner = IncrementalRunner(
            on_run_complete=self._on_incremental_run_complete,
            on_jstprove_range_fallback=self._on_jstprove_range_fallback,
            on_tile_onnx_fallback=self._on_tile_onnx_fallback,
        )
        self._incremental_runs: set[str] = set()
        self._incremental_run_circuits: dict[str, str] = {}
        self._incremental_runs_lock = threading.Lock()
        self._completed_run_statuses: dict[str, tuple[float, RunStatus]] = {}
        self._run_timings: dict[str, _RunTiming] = {}
        self._api_round_robin_idx = 0
        self.on_api_run_complete: (
            Callable[[str, str, bool, list[dict]], None] | None
        ) = None
        self._purge_old_runs()

    @staticmethod
    def _get_extraction_lock(key: str) -> threading.Lock:
        with _extraction_locks_guard:
            if key not in _extraction_locks:
                _extraction_locks[key] = threading.Lock()
            return _extraction_locks[key]

    @staticmethod
    def _ensure_slice_extracted(base_path: Path, slice_id: str) -> bool:
        slice_dir = base_path / slice_id
        if slice_dir.exists():
            return True
        dslice_path = base_path / f"{slice_id}.dslice"
        if not dslice_path.exists():
            return False
        lock_key = str(base_path / slice_id)
        lock = DSperseManager._get_extraction_lock(lock_key)
        with lock:
            if slice_dir.exists():
                return True
            staging_dir = base_path / f".{slice_id}.staging"
            try:
                if staging_dir.exists():
                    shutil.rmtree(staging_dir)
                logging.info(f"Extracting {slice_id} from {dslice_path}")
                Converter.extract_single_slice(base_path, slice_id, staging_dir)
                os.rename(str(staging_dir / slice_id), str(slice_dir))
                shutil.rmtree(staging_dir, ignore_errors=True)
                return True
            except OSError as e:
                if slice_dir.exists():
                    shutil.rmtree(staging_dir, ignore_errors=True)
                    return True
                logging.error(f"Failed to extract {slice_id} from {dslice_path}: {e}")
                if staging_dir.exists():
                    shutil.rmtree(staging_dir, ignore_errors=True)
                return False
            except Exception as e:
                logging.error(f"Failed to extract {slice_id} from {dslice_path}: {e}")
                if staging_dir.exists():
                    shutil.rmtree(staging_dir, ignore_errors=True)
                if slice_dir.exists():
                    shutil.rmtree(slice_dir, ignore_errors=True)
                return False

    @property
    def circuits(self) -> list[Circuit]:
        return [
            circuit
            for circuit in circuit_store.circuits.values()
            if circuit.metadata.type == CircuitType.DSPERSE_PROOF_GENERATION
        ]

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
            if self._loop is not None:
                asyncio.run_coroutine_threadsafe(coro, self._loop)
            else:
                logging.warning(
                    f"No event loop available, dropping coroutine: {coro!r}"
                )
                coro.close()

    def _get_circuit_by_id(self, circuit_id: str) -> Circuit:
        circuit = next((c for c in self.circuits if c.id == circuit_id), None)
        if circuit is None:
            circuit = circuit_store.ensure_circuit(circuit_id)
            if circuit.metadata.type != CircuitType.DSPERSE_PROOF_GENERATION:
                raise ValueError(
                    f"Circuit {circuit_id} is not a DSperse circuit (type: {circuit.metadata.type})"
                )
        return circuit

    def generate_dslice_requests(self) -> list[DSliceQueuedProofRequest]:
        if not self.circuits:
            return []
        return self.generate_incremental_request()

    def start_incremental_run(
        self,
        circuit: Circuit,
        inputs: dict | None = None,
        run_source: RunSource = RunSource.BENCHMARK,
        max_tiles: int | None = None,
    ) -> str:
        """
        Start an incremental run where miners compute outputs.

        Args:
            circuit: The circuit to execute
            inputs: Model inputs (generated if not provided)
            max_tiles: If set, cap the run to at most this many provable tiles

        Returns:
            Run UID
        """
        run_uid = self._incremental_runner.start_run(
            circuit, inputs, run_source, max_tiles=max_tiles
        )
        with self._incremental_runs_lock:
            self._incremental_runs.add(run_uid)
            self._incremental_run_circuits[run_uid] = circuit.id

        if self.event_client:
            status = self._incremental_runner.get_run_status(run_uid)
            self._schedule_async(
                self.event_client.emit_run_started(
                    run_uid=run_uid,
                    circuit_id=circuit.id,
                    circuit_name=circuit.metadata.name,
                    total_slices=status.total_slices if status else 0,
                    environment=capture_environment(),
                    total_tiles=status.total_tiles if status else 0,
                    slice_tile_counts=status.slice_tile_counts if status else None,
                    run_source=run_source.value,
                )
            )

        return run_uid

    def get_next_incremental_work(self, run_uid: str) -> list[DSliceQueuedProofRequest]:
        max_local_steps = 200
        for _ in range(max_local_steps):
            work_items = self._incremental_runner.get_next_work(run_uid)
            if work_items is None:
                return []
            if len(work_items) > 0:
                break
        else:
            logging.warning(
                f"get_next_incremental_work exhausted {max_local_steps} local ONNX steps for run {run_uid}"
            )
            return []

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
        with self._incremental_runs_lock:
            incremental_runs_snapshot = list(self._incremental_runs)
            active_circuit_ids = set(self._incremental_run_circuits.values())

        api_runs = [
            uid
            for uid in incremental_runs_snapshot
            if self._incremental_runner.get_run_source(uid) == RunSource.API
            and not self._incremental_runner.is_complete(uid)
        ]
        other_runs = [
            uid
            for uid in incremental_runs_snapshot
            if uid not in set(api_runs)
            and not self._incremental_runner.is_complete(uid)
        ]

        if api_runs:
            idx = self._api_round_robin_idx % len(api_runs)
            rotated = api_runs[idx:] + api_runs[:idx]
            self._api_round_robin_idx += 1
            for run_uid in rotated:
                requests = self.get_next_incremental_work(run_uid)
                if requests:
                    logging.info(
                        f"Generating {len(requests)} work items for run {run_uid}"
                    )
                    return requests

        for run_uid in other_runs:
            requests = self.get_next_incremental_work(run_uid)
            if requests:
                logging.info(f"Generating {len(requests)} work items for run {run_uid}")
                return requests

        available_circuits = [
            c for c in self.circuits if c.id not in active_circuit_ids
        ]
        if not available_circuits:
            return []
        circuit = random.choice(available_circuits)
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
        proof_system: ProofSystem | None = None,
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
            proof_system: Proof system used
            response_time_sec: Response time
            verification_time_sec: Verification time

        Returns:
            Tuple of (is_run_complete, next_slice_request)
        """
        task_id = (
            f"slice_{slice_num}" if not slice_num.startswith("slice_") else slice_num
        )

        timing = self._run_timings.setdefault(run_uid, _RunTiming())
        timing.total_response_time_sec += response_time_sec
        timing.total_verification_time_sec += verification_time_sec
        is_api = self._incremental_runner.get_run_source(run_uid) == RunSource.API
        timing.slices.append(
            _SliceMetric(
                slice_num=slice_num,
                response_time_sec=response_time_sec,
                verification_time_sec=verification_time_sec,
                success=success,
                is_tile=False,
                proof_data=proof if success and is_api else None,
                proof_system=proof_system,
            )
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
        proof_system: ProofSystem | None = None,
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
            proof_system: Proof system used
            response_time_sec: Response time
            verification_time_sec: Verification time

        Returns:
            Tuple of (is_run_complete, next_requests)
        """
        tile_slice_num = f"{slice_id.removeprefix('slice_')}_tile_{tile_idx}"
        timing = self._run_timings.setdefault(run_uid, _RunTiming())
        timing.total_response_time_sec += response_time_sec
        timing.total_verification_time_sec += verification_time_sec
        is_api = self._incremental_runner.get_run_source(run_uid) == RunSource.API
        timing.slices.append(
            _SliceMetric(
                slice_num=tile_slice_num,
                response_time_sec=response_time_sec,
                verification_time_sec=verification_time_sec,
                success=success,
                is_tile=True,
                proof_data=proof if success and is_api else None,
                proof_system=proof_system,
            )
        )

        slice_complete = self._incremental_runner.apply_result(
            run_uid=run_uid,
            task_id=task_id,
            success=success,
            output=computed_outputs,
            error=None if success else "Tile execution failed",
        )

        if self.event_client:
            slice_num = tile_slice_num
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

    def _on_tile_onnx_fallback(
        self, run_uid: str, slice_id: str, task_id: str, tile_idx: int
    ) -> None:
        logging.warning(
            f"Tile ONNX fallback in run {run_uid} slice {slice_id}: "
            f"task {task_id} (tile {tile_idx}) reverted to local ONNX"
        )
        if self.event_client:
            slice_num = (
                f"{slice_id.removeprefix('slice_')}_tile_{tile_idx}"
                if tile_idx >= 0
                else slice_id
            )
            self._schedule_async(
                self.event_client.emit_tile_onnx_fallback(
                    run_uid=run_uid,
                    slice_num=slice_num,
                    task_id=task_id,
                )
            )

    def abort_benchmark_runs(self) -> list[str]:
        with self._incremental_runs_lock:
            benchmark_uids = [
                uid
                for uid in self._incremental_runs
                if self._incremental_runner.get_run_source(uid) == RunSource.BENCHMARK
            ]
        aborted = []
        for run_uid in benchmark_uids:
            self._incremental_runner.abort_run(run_uid)
            aborted.append(run_uid)
        if aborted:
            logging.info(
                f"Aborted {len(aborted)} benchmark runs for incoming API request"
            )
        return aborted

    def abort_active_runs(self) -> list[str]:
        with self._incremental_runs_lock:
            active_uids = list(self._incremental_runs)
        aborted = []
        for run_uid in active_uids:
            state = self._incremental_runner._runs.get(run_uid)
            if state and not state.aborted and not state.is_complete:
                self._incremental_runner.abort_run(run_uid)
                aborted.append(run_uid)
        if aborted:
            logging.info(f"Aborted {len(aborted)} active runs for incoming request")
        return aborted

    def run_onnx_inference(self, circuit: Circuit, inputs: dict) -> Optional[Any]:
        return self._incremental_runner.run_onnx_inference(circuit, inputs)

    _COMPLETED_STATUS_TTL_SEC = 600

    def _evict_stale_statuses(self) -> None:
        cutoff = time.monotonic() - self._COMPLETED_STATUS_TTL_SEC
        stale = [
            k for k, (ts, _) in self._completed_run_statuses.items() if ts < cutoff
        ]
        for k in stale:
            del self._completed_run_statuses[k]

    def _on_incremental_run_complete(self, run_uid: str, success: bool) -> None:
        logging.info(f"Incremental run {run_uid} completed, success={success}")

        with self._incremental_runs_lock:
            circuit_id = self._incremental_run_circuits.get(run_uid)

        status = self._incremental_runner.get_run_status(run_uid)
        if status:
            self._completed_run_statuses[run_uid] = (time.monotonic(), status)
            self._evict_stale_statuses()

        if self.event_client and status:
            self._schedule_async(
                self.event_client.emit_run_complete(
                    run_uid=run_uid,
                    all_successful=success,
                    total_run_time_sec=status.elapsed_time,
                )
            )

        run_source = self._incremental_runner.get_run_source(run_uid)
        output_tensor = None
        if success:
            output_tensor = self._incremental_runner.get_final_output(run_uid)

        self._incremental_runner.cleanup_run(run_uid)
        with self._incremental_runs_lock:
            self._incremental_runs.discard(run_uid)
            self._incremental_run_circuits.pop(run_uid, None)

        run_timing = self._run_timings.pop(run_uid, _RunTiming())

        if status and circuit_id:
            threading.Thread(
                target=self._submit_metrics,
                args=(run_uid, circuit_id, success, status, run_timing),
                daemon=True,
            ).start()

        if run_source == RunSource.API and circuit_id:
            proof_artifacts = [
                {
                    "slice_num": s.slice_num,
                    "proof_system": (
                        s.proof_system.value
                        if s.proof_system
                        else ProofSystem.JSTPROVE.value
                    ),
                    "proof_data": s.proof_data,
                    "parent_slice": (
                        s.slice_num.split("_tile_")[0] if s.is_tile else None
                    ),
                    "tile_idx": (
                        int(s.slice_num.split("_tile_")[1]) if s.is_tile else None
                    ),
                }
                for s in run_timing.slices
                if s.success and s.proof_data is not None
            ]

            if output_tensor is not None:
                from execution_layer.proof_uploader import upload_final_output

                final_output = (
                    output_tensor.tolist()
                    if hasattr(output_tensor, "tolist")
                    else output_tensor
                )
                threading.Thread(
                    target=upload_final_output,
                    args=(run_uid, circuit_id, {"output_data": final_output}),
                    daemon=True,
                ).start()

            if self.on_api_run_complete:
                try:
                    self.on_api_run_complete(
                        run_uid, circuit_id, success, proof_artifacts
                    )
                except Exception as e:
                    logging.error(f"on_api_run_complete callback failed: {e}")

    def is_incremental_run(self, run_uid: str) -> bool:
        with self._incremental_runs_lock:
            return run_uid in self._incremental_runs

    def get_run_status(self, run_uid: str) -> RunStatus | None:
        entry = self._completed_run_statuses.get(run_uid)
        if entry:
            return entry[1]
        return self._incremental_runner.get_run_status(run_uid)

    def _submit_metrics(
        self,
        run_uid: str,
        circuit_id: str,
        all_successful: bool,
        status: RunStatus,
        run_timing: _RunTiming,
    ):
        try:
            import httpx

            circuit = next((c for c in self.circuits if c.id == circuit_id), None)
            circuit_name = circuit.metadata.name if circuit else circuit_id

            circuit_slices = sum(1 for s in run_timing.slices if s.success)
            onnx_slices = status.total_slices - len(
                {s.slice_num.split("_tile_")[0] for s in run_timing.slices}
            )

            slice_metrics = [
                {
                    "slice_num": s.slice_num,
                    "proof_system": (
                        s.proof_system.value
                        if s.proof_system
                        else ProofSystem.JSTPROVE.value
                    ),
                    "backend_used": (
                        s.proof_system.value
                        if s.proof_system
                        else ProofSystem.JSTPROVE.value
                    ),
                    "witness_time_sec": 0.0,
                    "response_time_sec": s.response_time_sec,
                    "verification_time_sec": s.verification_time_sec,
                    "is_tiled": s.is_tile,
                    "success": s.success,
                }
                for s in run_timing.slices
            ]

            payload = {
                "run_uid": run_uid,
                "validator_key": self._get_validator_hotkey(),
                "circuit_id": circuit_id,
                "circuit_name": circuit_name,
                "total_slices": status.total_slices,
                "circuit_slices": circuit_slices,
                "onnx_slices": max(onnx_slices, 0),
                "total_witness_time_sec": 0.0,
                "total_response_time_sec": run_timing.total_response_time_sec,
                "total_verification_time_sec": run_timing.total_verification_time_sec,
                "total_run_time_sec": status.elapsed_time,
                "all_successful": all_successful,
                "failed_slice_count": status.failed,
                "environment": capture_environment(),
                "slices": slice_metrics,
            }

            api_url = getattr(cli_parser.config, "sn2_api_url", None)
            if not api_url:
                api_url = "https://sn2-api.inferencelabs.com"

            hotkey = self._get_validator_hotkey()
            body = json.dumps(payload)
            signature = self._sign_request(body)

            if hotkey == "unknown" or not signature:
                logging.warning(
                    f"Skipping metrics submission for run {run_uid}: "
                    f"invalid hotkey or signature"
                )
                return

            with httpx.Client(timeout=30.0) as client:
                response = client.post(
                    f"{api_url}/statistics/dsperse/log/",
                    content=body,
                    headers={
                        "Content-Type": "application/json",
                        "X-Request-Signature": signature,
                    },
                )
                if response.status_code == 200:
                    logging.debug(f"Metrics submitted for run {run_uid}")
                else:
                    logging.warning(
                        f"Failed to submit metrics for run {run_uid}: "
                        f"{response.status_code} - {response.text}"
                    )
        except Exception as e:
            logging.warning(f"Failed to submit dsperse metrics: {e}")

    def _get_validator_hotkey(self) -> str:
        try:
            wallet = bt.Wallet(config=cli_parser.config)
            return wallet.hotkey.ss58_address
        except Exception:
            return "unknown"

    def _sign_request(self, body: str) -> str:
        try:
            import base64

            wallet = bt.Wallet(config=cli_parser.config)
            signature = wallet.hotkey.sign(body.encode())
            return base64.b64encode(signature).decode()
        except Exception:
            return ""

    def prove_slice(
        self,
        circuit_id: str,
        slice_num: str,
        inputs: dict,
        outputs: dict | None,
        proof_system: ProofSystem,
    ) -> dict:
        """
        Generate proof for a slice using JSTprove.

        In incremental mode (outputs=None), inference is run to compute outputs,
        which are returned in the result.
        """
        incremental_mode = outputs is None
        circuit = self._get_circuit_by_id(circuit_id)
        base_slice_num, tile_idx = self._parse_slice_num(slice_num)
        base_path = Path(circuit.paths.base_path)
        slice_id = f"slice_{base_slice_num}"
        model_dir = base_path / slice_id
        result = {
            "circuit_id": circuit_id,
            "slice_num": slice_num,
            "success": False,
            "proof_generation_time": None,
            "proof": None,
        }
        if not self._ensure_slice_extracted(base_path, slice_id):
            logging.error(
                f"Slice directory {model_dir} does not exist and no .dslice archive found"
            )
            return result

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

            if proof_system != ProofSystem.JSTPROVE:
                logging.error(f"Unsupported proof system: {proof_system}")
                return result

            jst_model_path = self._find_jstprove_circuit(
                model_dir, base_slice_num, is_tiled=False
            )
            if jst_model_path is None or not jst_model_path.exists():
                logging.error(f"JSTprove circuit not found for slice {slice_num}")
                return result

            prove_start = time.time()
            success, proof_data, witness_data, _ = self._jstprove_witness_and_prove(
                jst_model_path,
                input_file,
                output_file,
                tmp_path,
                f"slice {slice_num}",
            )
            if incremental_mode and witness_data is not None:
                result["witness"] = witness_data
            if not success:
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
        base_path = Path(circuit.paths.base_path)
        slice_id = f"slice_{base_slice_num}"
        slice_dir = base_path / slice_id
        metadata_path = slice_dir / "metadata.json"

        if not metadata_path.exists():
            if not self._ensure_slice_extracted(base_path, slice_id):
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
        self._incremental_runner.cleanup_run(run_uid)
        self._completed_run_statuses.pop(run_uid, None)
        self._run_timings.pop(run_uid, None)
        with self._incremental_runs_lock:
            self._incremental_runs.discard(run_uid)
            self._incremental_run_circuits.pop(run_uid, None)

    def total_cleanup(self):
        logging.info("Performing total cleanup of all DSperse run data...")
        with self._incremental_runs_lock:
            run_uids = list(self._incremental_runs)
        for run_uid in run_uids:
            try:
                self._incremental_runner.cleanup_run(run_uid)
            except Exception as e:
                logging.error(f"Failed to cleanup run {run_uid}: {e}")
        with self._incremental_runs_lock:
            self._incremental_runs.clear()
            self._incremental_run_circuits.clear()
        self._completed_run_statuses.clear()
        self._run_timings.clear()
