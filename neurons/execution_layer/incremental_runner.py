"""
IncrementalRunner for distributed slice execution in the validator.

Simple state machine:
1. tensor_cache is source of truth for layer completion
2. For each slice: inputs ready? → extract if needed → ONNX locally OR JSTprove to miners
3. Tiled/non-tiled is just N=1 vs N=num_tiles, same flow
"""

import json
import secrets
import shutil
import time
import zipfile
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any, Callable, Optional

import torch
from bittensor import logging

from dsperse.src.analyzers.schema import (
    Backend,
    TilingInfo,
    RunSliceMetadata,
    RunMetadata,
)
from dsperse.src.run.runner import Runner as DsperseRunner
from dsperse.src.run.tile_executor import TileExecutor
from dsperse.src.slice.utils.converter import Converter
from constants import RunSource
from execution_layer.circuit import Circuit, ProofSystem
from _validator.models.dslice_request import DSliceQueuedProofRequest
from _validator.models.request_type import RequestType


@dataclass
class WorkItem:
    """A single work item (slice or tile) to be processed."""

    task_id: str
    slice_id: str
    tile_idx: Optional[int]
    inputs: dict
    proof_system: ProofSystem
    circuit: Circuit
    run_uid: str


@dataclass
class RunState:
    """State of an incremental run."""

    run_uid: str
    circuit: Circuit
    slices_path: Path
    tensor_cache: dict[str, Any]
    execution_order: list[str]
    slice_metadata: dict[str, RunSliceMetadata]
    total_tiles: int = 0
    slice_tile_counts: dict[str, int] = field(default_factory=dict)
    current_idx: int = 0
    pending_work: dict[str, bool] = field(default_factory=dict)
    completed_slices: list[str] = field(default_factory=list)
    failed_slices: list[str] = field(default_factory=list)
    failed_tasks: set[str] = field(default_factory=set)
    onnx_fallback_tasks: set[str] = field(default_factory=set)
    run_source: RunSource = RunSource.BENCHMARK
    start_time: float = 0.0
    aborted: bool = False

    @property
    def current_slice_id(self) -> Optional[str]:
        if self.current_idx >= len(self.execution_order):
            return None
        return self.execution_order[self.current_idx]

    @property
    def is_complete(self) -> bool:
        if self.aborted:
            return True
        return self.current_idx >= len(self.execution_order) and not self.pending_work

    @property
    def is_waiting(self) -> bool:
        return bool(self.pending_work)


class IncrementalRunner:
    """
    Simple incremental runner for distributed slice execution.

    Core loop:
        work_items = runner.get_next_work(run_uid)
        if work_items is None:
            # Waiting for pending work or complete
            pass
        elif len(work_items) == 0:
            # ONNX slice executed locally, call again
            pass
        else:
            # Send work_items to miners
            for item in work_items:
                send_to_miner(item)
    """

    JSTPROVE_N_BITS = 32
    JSTPROVE_RANGE_LIMIT = 2 ** (JSTPROVE_N_BITS - 1)

    def __init__(
        self,
        on_run_complete: Optional[Callable[[str, bool], None]] = None,
        on_jstprove_range_fallback: Optional[Callable[[str, str, dict], None]] = None,
        on_tile_onnx_fallback: Optional[Callable[[str, str, str, int], None]] = None,
    ):
        self._runs: dict[str, RunState] = {}
        self._on_run_complete = on_run_complete
        self._on_jstprove_range_fallback = on_jstprove_range_fallback
        self._on_tile_onnx_fallback = on_tile_onnx_fallback

    def _build_from_dslice_zips(
        self, slices_path: Path, dslice_files: list[Path]
    ) -> dict:
        from dsperse.src.analyzers.runner_analyzer import RunnerAnalyzer
        from dsperse.src.analyzers.schema import RunMetadata as DsperseRunMetadata

        slices_data = []
        for dslice_path in sorted(dslice_files):
            try:
                with zipfile.ZipFile(dslice_path, "r") as zf:
                    if "metadata.json" not in zf.namelist():
                        continue
                    with zf.open("metadata.json") as f:
                        meta = json.load(f)
                        if meta.get("slices"):
                            slice_meta = meta["slices"][0]
                            slice_meta["slice_id"] = dslice_path.stem
                            slices_data.append(slice_meta)
            except zipfile.BadZipFile:
                logging.warning(f"Corrupt dslice file {dslice_path}, skipping")
                continue

        slices = RunnerAnalyzer.process_slices(slices_path, slices_data)
        run_meta = DsperseRunMetadata(
            slices=slices,
            execution_chain=RunnerAnalyzer._build_execution_chain(slices),
            circuit_slices=RunnerAnalyzer._build_circuit_slices(slices),
            overall_security=RunnerAnalyzer._calculate_security(slices),
        )
        return run_meta.to_dict()

    def start_run(
        self,
        circuit: Circuit,
        inputs: Optional[dict] = None,
        run_source: RunSource = RunSource.BENCHMARK,
    ) -> str:
        """Start a new incremental run."""
        run_uid = f"{datetime.now().strftime('%Y%m%d%H%M%S%f')}-{secrets.token_hex(8)}"
        logging.info(f"Starting incremental run {run_uid} for {circuit.metadata.name}")

        if inputs is None:
            inputs = circuit.input_handler(RequestType.BENCHMARK).generate()

        slices_path = Path(circuit.paths.base_path)

        from dsperse.src.analyzers.runner_analyzer import RunnerAnalyzer

        dslice_files = list(slices_path.glob("*.dslice"))
        valid_dslice_files = [f for f in dslice_files if zipfile.is_zipfile(f)]
        if valid_dslice_files != dslice_files:
            corrupt = set(dslice_files) - set(valid_dslice_files)
            for path in corrupt:
                logging.warning(
                    f"Corrupt dslice file, removing for re-download: {path}"
                )
                try:
                    path.unlink(missing_ok=True)
                except OSError as e:
                    logging.warning(f"Failed to remove corrupt dslice {path}: {e}")
        if valid_dslice_files:
            run_metadata_dict = self._build_from_dslice_zips(
                slices_path, valid_dslice_files
            )
        elif RunnerAnalyzer._has_model_metadata(slices_path):
            slices_metadata = RunnerAnalyzer.load_slices_metadata(slices_path)
            run_metadata_dict = RunnerAnalyzer.build_run_metadata(
                slices_path, slices_metadata
            )
        else:
            run_metadata_dict = RunnerAnalyzer._build_from_per_slice_dirs(slices_path)
        run_metadata = RunMetadata.from_dict(run_metadata_dict)

        execution_order = sorted(
            run_metadata.slices.keys(), key=lambda k: int(k.split("_")[1])
        )

        tensor_cache = {}
        if isinstance(inputs, dict):
            input_tensor = None
            for key in ["input_data", "input", "data", "inputs"]:
                if key in inputs:
                    input_tensor = torch.tensor(inputs[key])
                    break
            if input_tensor is None:
                input_tensor = torch.tensor(list(inputs.values())[0])
        else:
            input_tensor = (
                torch.tensor(inputs) if not isinstance(inputs, torch.Tensor) else inputs
            )

        first_slice = (
            run_metadata.slices.get(execution_order[0]) if execution_order else None
        )
        if first_slice:
            input_names = first_slice.dependencies.filtered_inputs
            if input_names:
                tensor_cache[input_names[0]] = input_tensor

        state = RunState(
            run_uid=run_uid,
            circuit=circuit,
            slices_path=slices_path,
            tensor_cache=tensor_cache,
            execution_order=execution_order,
            slice_metadata={k: v for k, v in run_metadata.slices.items()},
            run_source=run_source,
            start_time=time.perf_counter(),
        )

        total_tiles = 0
        slice_tile_counts: dict[str, int] = {}
        for slice_id in execution_order:
            meta = run_metadata.slices.get(slice_id)
            if meta and self._has_circuits(state, slice_id, meta):
                count = (
                    meta.tiling.num_tiles
                    if meta.tiling and meta.tiling.num_tiles > 1
                    else 1
                )
                slice_tile_counts[slice_id] = count
                total_tiles += count
        state.total_tiles = total_tiles
        state.slice_tile_counts = slice_tile_counts

        self._runs[run_uid] = state
        logging.info(
            f"Run {run_uid} initialized with {len(execution_order)} slices, {total_tiles} tiles"
        )
        return run_uid

    def get_next_work(self, run_uid: str) -> Optional[list[WorkItem]]:
        """
        Get the next work items to process.

        Returns:
            None - waiting for pending work or run complete
            [] - ONNX slice executed locally, call again for next slice
            [WorkItem, ...] - work items to send to miners
        """
        if run_uid not in self._runs:
            logging.warning(f"Run {run_uid} not found")
            return None

        state = self._runs[run_uid]

        if state.is_waiting:
            return None

        if state.is_complete:
            return None

        slice_id = state.current_slice_id
        if slice_id is None:
            return None

        meta = state.slice_metadata.get(slice_id)
        if not meta:
            logging.error(f"No metadata for {slice_id}")
            state.failed_slices.append(slice_id)
            state.aborted = True
            logging.error(f"Run {state.run_uid} aborted: no metadata for {slice_id}")
            self._on_complete(state)
            return None

        required_inputs = meta.dependencies.filtered_inputs
        missing = [i for i in required_inputs if i not in state.tensor_cache]
        if missing:
            logging.error(f"Slice {slice_id} missing inputs: {missing}")
            state.failed_slices.append(slice_id)
            state.aborted = True
            logging.error(f"Run {state.run_uid} aborted: missing inputs for {slice_id}")
            self._on_complete(state)
            return None

        self._ensure_extracted(state, slice_id)

        is_tiled = meta.tiling and meta.tiling.num_tiles > 1
        has_circuits = self._has_circuits(state, slice_id, meta)

        if not has_circuits:
            self._run_onnx_locally(state, slice_id, meta)
            self._cleanup_extracted_slice(state, slice_id)
            state.completed_slices.append(slice_id)
            state.current_idx += 1
            return []

        overflow_info = self._preflight_jstprove_range_check(state, slice_id, meta)
        if overflow_info:
            logging.warning(
                f"Slice {slice_id}: {overflow_info['overflow_count']}/{overflow_info['total_elements']} "
                f"outputs exceed JSTprove {self.JSTPROVE_N_BITS}-bit range "
                f"(max |val|={overflow_info['max_abs']:.0f}, limit={self.JSTPROVE_RANGE_LIMIT}), "
                f"falling back to ONNX"
            )
            self._cleanup_extracted_slice(state, slice_id)
            state.completed_slices.append(slice_id)
            state.current_idx += 1
            if self._on_jstprove_range_fallback:
                self._on_jstprove_range_fallback(state.run_uid, slice_id, overflow_info)
            return []

        return self._create_work_items(state, slice_id, meta, is_tiled)

    def apply_result(
        self,
        run_uid: str,
        task_id: str,
        success: bool,
        output: Optional[torch.Tensor] = None,
        error: Optional[str] = None,
    ) -> bool:
        """
        Apply result from a miner.

        Returns True if this was the last pending item for current slice.
        """
        if run_uid not in self._runs:
            return False

        state = self._runs[run_uid]

        if task_id not in state.pending_work:
            logging.warning(f"Task {task_id} not in pending work")
            return False

        del state.pending_work[task_id]

        if not success:
            logging.warning(f"Task {task_id} failed from miner: {error}")
            try:
                self._run_onnx_for_failed_task(state, task_id)
                state.onnx_fallback_tasks.add(task_id)
                logging.warning(f"ONNX fallback succeeded for {task_id}")
                if self._on_tile_onnx_fallback:
                    tile_idx = (
                        int(task_id.split("_tile_")[1]) if "_tile_" in task_id else -1
                    )
                    self._on_tile_onnx_fallback(
                        run_uid, state.current_slice_id, task_id, tile_idx
                    )
            except Exception as onnx_err:
                logging.error(f"ONNX fallback also failed for {task_id}: {onnx_err}")
                state.failed_tasks.add(task_id)
                if not state.pending_work:
                    state.failed_slices.append(state.current_slice_id)
                    state.aborted = True
                    logging.error(
                        f"Run {run_uid} aborted: slice {state.current_slice_id} failed"
                    )
                    self._on_complete(state)
                return not state.pending_work

        slice_id = state.current_slice_id
        meta = state.slice_metadata.get(slice_id)

        if "_tile_" in task_id:
            tile_idx = int(task_id.split("_tile_")[1])
            self._store_tile_output(state, meta, tile_idx, output)
        else:
            self._store_slice_output(state, meta, output)

        if not state.pending_work:
            if state.failed_tasks:
                logging.error(
                    f"Slice {slice_id} has {len(state.failed_tasks)} failed tasks, aborting run"
                )
                state.failed_slices.append(slice_id)
                state.aborted = True
                self._on_complete(state)
                return True

            try:
                if meta and meta.tiling and meta.tiling.num_tiles > 1:
                    self._reconstruct_from_tiles(state, slice_id, meta.tiling)
                    self._cleanup_tile_cache(state, meta.tiling)
                self._cleanup_extracted_slice(state, slice_id)
                state.completed_slices.append(slice_id)
                state.current_idx += 1
                state.failed_tasks.clear()

                if state.is_complete:
                    self._on_complete(state)
            except Exception as e:
                logging.error(f"Error completing slice {slice_id}: {e}")
                state.failed_slices.append(slice_id)
                state.aborted = True
                self._on_complete(state)

            return True

        return False

    def get_run_status(self, run_uid: str) -> Optional[dict]:
        """Get run status."""
        if run_uid not in self._runs:
            return None
        state = self._runs[run_uid]
        return {
            "run_uid": run_uid,
            "circuit_name": state.circuit.metadata.name,
            "total_slices": len(state.execution_order),
            "total_tiles": state.total_tiles,
            "slice_tile_counts": state.slice_tile_counts,
            "current_slice": state.current_slice_id,
            "completed": len(state.completed_slices),
            "failed": len(state.failed_slices),
            "pending_work": len(state.pending_work),
            "is_complete": state.is_complete,
            "elapsed_time": time.perf_counter() - state.start_time,
        }

    def get_final_output(self, run_uid: str) -> Optional[Any]:
        """Get final output tensor."""
        if run_uid not in self._runs:
            return None
        state = self._runs[run_uid]
        if not state.is_complete:
            return None

        last_slice = state.execution_order[-1] if state.execution_order else None
        if not last_slice:
            return None
        meta = state.slice_metadata.get(last_slice)
        if not meta:
            return None

        output_names = meta.dependencies.output
        for name in output_names:
            if name in state.tensor_cache:
                return state.tensor_cache[name]
        return None

    def is_complete(self, run_uid: str) -> bool:
        """Check if run is complete."""
        if run_uid not in self._runs:
            return False
        return self._runs[run_uid].is_complete

    def has_pending_work(self, run_uid: str) -> bool:
        """Check if run has work currently being processed."""
        if run_uid not in self._runs:
            return False
        return self._runs[run_uid].is_waiting

    def abort_run(self, run_uid: str) -> None:
        if run_uid not in self._runs:
            return
        state = self._runs[run_uid]
        if state.aborted or state.is_complete:
            return
        state.aborted = True
        logging.warning(f"Run {run_uid} aborted (preempted)")
        self._on_complete(state)

    def get_run_source(self, run_uid: str) -> RunSource | None:
        if run_uid not in self._runs:
            return None
        return self._runs[run_uid].run_source

    def cleanup_run(self, run_uid: str) -> None:
        """Clean up run state."""
        if run_uid in self._runs:
            del self._runs[run_uid]

    def create_queued_request(self, work_item: WorkItem) -> DSliceQueuedProofRequest:
        """Convert WorkItem to DSliceQueuedProofRequest."""
        slice_num = work_item.slice_id.replace("slice_", "")
        if work_item.tile_idx is not None:
            slice_num = f"{slice_num}_tile_{work_item.tile_idx}"

        return DSliceQueuedProofRequest(
            circuit=work_item.circuit,
            inputs=work_item.inputs,
            outputs=None,
            slice_num=slice_num,
            run_uid=work_item.run_uid,
            proof_system=work_item.proof_system,
            is_tile=work_item.tile_idx is not None,
            tile_idx=work_item.tile_idx,
            task_id=work_item.task_id,
        )

    def _ensure_extracted(self, state: RunState, slice_id: str) -> None:
        """Extract slice from .dslice if not already extracted."""
        slice_dir = state.slices_path / slice_id
        dslice_path = state.slices_path / f"{slice_id}.dslice"

        if not slice_dir.exists() and dslice_path.exists():
            logging.info(f"Extracting {slice_id} from {dslice_path}")
            Converter.extract_single_slice(
                state.slices_path, slice_id, state.slices_path
            )

    def _cleanup_extracted_slice(self, state: RunState, slice_id: str) -> None:
        """Remove extracted slice folder if .dslice exists (lazy mode cleanup)."""
        slice_dir = state.slices_path / slice_id
        dslice_path = state.slices_path / f"{slice_id}.dslice"

        if slice_dir.exists() and dslice_path.exists():
            logging.info(f"Cleaning up extracted {slice_id}")
            shutil.rmtree(slice_dir, ignore_errors=True)

    def _has_circuits(
        self, state: RunState, slice_id: str, meta: RunSliceMetadata
    ) -> bool:
        """Check if slice has JSTprove circuits by checking actual files.

        Checks both current and legacy paths for backwards compatibility:
        - Tiled: jstprove/tiles/tile_circuit.txt (current)
        - Non-tiled: payload/jstprove/{slice_id}_circuit.txt (legacy, used by dsperse_manager.py)
        """
        if state.circuit.metadata.proof_system != ProofSystem.JSTPROVE:
            return False

        is_tiled = meta.tiling and meta.tiling.num_tiles > 1
        circuit_paths = (
            [
                "jstprove/tiles/tile_circuit.txt",
                "payload/jstprove/tiles/tile_circuit.txt",
            ]
            if is_tiled
            else [
                f"jstprove/{slice_id}_circuit.txt",
                f"payload/jstprove/{slice_id}_circuit.txt",
            ]
        )

        slice_dir = state.slices_path / slice_id
        dslice_zip = state.slices_path / f"{slice_id}.dslice"

        for rel in circuit_paths:
            if (slice_dir / rel).exists():
                return True

        if dslice_zip.exists():
            try:
                with zipfile.ZipFile(dslice_zip, "r") as zf:
                    for rel in circuit_paths:
                        if rel in zf.namelist():
                            return True
            except zipfile.BadZipFile:
                logging.warning(
                    f"Corrupt dslice file {dslice_zip}, skipping circuit check"
                )

        return False

    def _preflight_jstprove_range_check(
        self, state: RunState, slice_id: str, meta: RunSliceMetadata
    ) -> Optional[dict]:
        """Run ONNX locally and check if outputs fit JSTprove's N_BITS range.

        Returns overflow info dict if any values exceed the range, None otherwise.
        On overflow, the ONNX results are already stored in tensor_cache
        so the slice can be completed without miner dispatch.
        """
        self._run_onnx_locally(state, slice_id, meta)

        output_names = meta.dependencies.output
        for name in output_names:
            if name not in state.tensor_cache:
                continue
            tensor = state.tensor_cache[name]
            if not isinstance(tensor, torch.Tensor):
                tensor = torch.tensor(tensor)
            abs_vals = tensor.abs()
            max_abs = abs_vals.max().item()
            if max_abs >= self.JSTPROVE_RANGE_LIMIT:
                overflow_count = int(
                    (abs_vals >= self.JSTPROVE_RANGE_LIMIT).sum().item()
                )
                return {
                    "n_bits": self.JSTPROVE_N_BITS,
                    "limit": self.JSTPROVE_RANGE_LIMIT,
                    "max_abs": max_abs,
                    "overflow_count": overflow_count,
                    "total_elements": tensor.numel(),
                    "slice_id": slice_id,
                    "circuit_id": state.circuit.id,
                }
        return None

    def _run_onnx_for_failed_task(
        self, state: RunState, task_id: str
    ) -> Optional[torch.Tensor]:
        from dsperse.src.run.utils.runner_utils import RunnerUtils
        from dsperse.src.backends.onnx_models import OnnxModels

        slice_id = state.current_slice_id
        meta = state.slice_metadata.get(slice_id)
        if not meta:
            raise ValueError(f"No metadata for {slice_id}")

        if "_tile_" in task_id:
            if not meta.tiling:
                raise ValueError(f"Missing tiling metadata for slice {slice_id}")
            tile_idx = int(task_id.split("_tile_")[1])
            cache_name = f"tile_{meta.tiling.slice_idx}_{tile_idx}_in"
            tile_input = state.tensor_cache.get(cache_name)
            if tile_input is None:
                raise ValueError(f"Missing tile input {cache_name} for ONNX fallback")

            onnx_path = RunnerUtils.resolve_relative_path(
                meta.path, state.slices_path / slice_id
            )
            if not onnx_path or not Path(onnx_path).exists():
                raise ValueError(f"ONNX path not found for {slice_id}: {onnx_path}")

            logging.warning(f"Running ONNX fallback for tile {task_id}")
            success, result = OnnxModels.run_inference_tensor(
                input_tensor=tile_input,
                model_path=onnx_path,
            )
            if not success:
                raise RuntimeError(
                    f"ONNX inference failed for tile {task_id}: {result}"
                )

            output_tensors = result.get("output_tensors", {})
            if output_tensors:
                output_tensor = next(iter(output_tensors.values()))
            else:
                output_tensor = RunnerUtils.extract_output_tensor(result)

            if output_tensor is None:
                raise RuntimeError(f"No output tensor from ONNX fallback for {task_id}")

            self._store_tile_output(state, meta, tile_idx, output_tensor)
            return output_tensor
        else:
            logging.warning(f"Running ONNX fallback for slice {task_id}")
            self._run_single_onnx(state, slice_id, meta)
            return None

    def _run_onnx_locally(
        self, state: RunState, slice_id: str, meta: RunSliceMetadata
    ) -> None:
        """Run slice locally using ONNX."""
        logging.info(f"Running {slice_id} locally with ONNX")

        is_tiled = meta.tiling and meta.tiling.num_tiles > 1

        if is_tiled:
            self._run_tiled_onnx(state, slice_id, meta)
        else:
            self._run_single_onnx(state, slice_id, meta)

    def _run_single_onnx(
        self, state: RunState, slice_id: str, meta: RunSliceMetadata
    ) -> None:
        """Run a single non-tiled slice with ONNX."""
        from dsperse.src.run.utils.runner_utils import RunnerUtils
        from dsperse.src.backends.onnx_models import OnnxModels

        onnx_path = RunnerUtils.resolve_relative_path(
            meta.path, state.slices_path / slice_id
        )

        if not onnx_path or not Path(onnx_path).exists():
            raise ValueError(f"ONNX path not found for {slice_id}: {onnx_path}")

        input_tensor = None
        for name in meta.dependencies.filtered_inputs:
            if name in state.tensor_cache:
                input_tensor = state.tensor_cache[name]
                break

        if input_tensor is None:
            raise ValueError(f"No input tensor for {slice_id}")

        success, result = OnnxModels.run_inference_tensor(
            input_tensor=input_tensor,
            model_path=onnx_path,
        )

        if not success:
            raise RuntimeError(f"ONNX inference failed for {slice_id}: {result}")

        output_tensors = result.get("output_tensors", {})
        if not output_tensors:
            output_tensor = RunnerUtils.extract_output_tensor(result)
            if output_tensor is None:
                raise RuntimeError(f"No output tensor from {slice_id}")
            output_names = meta.dependencies.output
            if output_names:
                state.tensor_cache[output_names[-1]] = output_tensor
                logging.info(f"Stored output '{output_names[-1]}' in tensor_cache")
        else:
            output_names = set(meta.dependencies.output)
            stored = 0
            for name, tensor in output_tensors.items():
                if name in output_names:
                    state.tensor_cache[name] = tensor
                    stored += 1
            logging.info(f"Stored {stored} outputs for {slice_id} in tensor_cache")

    def _run_tiled_onnx(
        self, state: RunState, slice_id: str, meta: RunSliceMetadata
    ) -> None:
        """Run a tiled slice with ONNX."""
        tiling = meta.tiling

        tile_executor = TileExecutor(state.slices_path, state.tensor_cache)
        input_tensor = tile_executor.get_input_tensor(slice_id, tiling, meta)
        tile_executor.split_into_tiles(slice_id, tiling, input_tensor)

        runner = DsperseRunner(batch=True)
        runner.slices_path = state.slices_path
        runner.tensor_cache = state.tensor_cache

        run_dir = state.slices_path / "runs" / state.run_uid
        run_dir.mkdir(parents=True, exist_ok=True)

        runner.run_tiles(
            slice_id=slice_id,
            tiling=tiling,
            meta=meta,
            run_dir=run_dir,
            tensor_cache=state.tensor_cache,
            backend=Backend.ONNX,
        )

        tile_executor.reconstruct_from_tiles(slice_id, tiling)
        self._cleanup_tile_cache(state, tiling)
        logging.info(f"Completed {tiling.num_tiles} tiles for {slice_id}")

    def _create_work_items(
        self, state: RunState, slice_id: str, meta: RunSliceMetadata, is_tiled: bool
    ) -> list[WorkItem]:
        """Create work items to send to miners."""
        work_items = []

        if is_tiled:
            tiling = meta.tiling
            tile_executor = TileExecutor(state.slices_path, state.tensor_cache)
            input_tensor = tile_executor.get_input_tensor(slice_id, tiling, meta)
            tile_executor.split_into_tiles(slice_id, tiling, input_tensor)

            for tile_idx in range(tiling.num_tiles):
                cache_name = f"tile_{tiling.slice_idx}_{tile_idx}_in"
                tile_tensor = state.tensor_cache.get(cache_name)
                if tile_tensor is None:
                    logging.error(f"Missing tile input {cache_name}")
                    continue

                task_id = f"{slice_id}_tile_{tile_idx}"
                work_items.append(
                    WorkItem(
                        task_id=task_id,
                        slice_id=slice_id,
                        tile_idx=tile_idx,
                        inputs={"input_data": tile_tensor.tolist()},
                        proof_system=ProofSystem.JSTPROVE,
                        circuit=state.circuit,
                        run_uid=state.run_uid,
                    )
                )
                state.pending_work[task_id] = True
        else:
            input_tensor = None
            for name in meta.dependencies.filtered_inputs:
                if name in state.tensor_cache:
                    input_tensor = state.tensor_cache[name]
                    break

            if input_tensor is None:
                logging.error(f"No input tensor for {slice_id}")
                return []

            task_id = slice_id
            work_items.append(
                WorkItem(
                    task_id=task_id,
                    slice_id=slice_id,
                    tile_idx=None,
                    inputs={"input_data": input_tensor.tolist()},
                    proof_system=ProofSystem.JSTPROVE,
                    circuit=state.circuit,
                    run_uid=state.run_uid,
                )
            )
            state.pending_work[task_id] = True

        logging.info(f"Created {len(work_items)} work items for {slice_id}")
        return work_items

    def _store_tile_output(
        self,
        state: RunState,
        meta: RunSliceMetadata,
        tile_idx: int,
        output: Optional[torch.Tensor],
    ) -> None:
        """Store tile output in tensor_cache."""
        if not meta or not meta.tiling or output is None:
            return

        cache_name = f"tile_{meta.tiling.slice_idx}_{tile_idx}_out"
        state.tensor_cache[cache_name] = output

    def _store_slice_output(
        self, state: RunState, meta: RunSliceMetadata, output: Optional[torch.Tensor]
    ) -> None:
        """Store slice output in tensor_cache."""
        if not meta or output is None:
            return

        output_names = meta.dependencies.output
        if output_names:
            state.tensor_cache[output_names[0]] = output

    def _reconstruct_from_tiles(
        self, state: RunState, slice_id: str, tiling: TilingInfo
    ) -> None:
        """Reconstruct full output from tile outputs."""
        tile_executor = TileExecutor(state.slices_path, state.tensor_cache)
        tile_executor.reconstruct_from_tiles(slice_id, tiling)

    def _cleanup_tile_cache(self, state: RunState, tiling: TilingInfo) -> None:
        """Remove tile_X_Y_in and tile_X_Y_out entries from tensor_cache after reconstruction."""
        slice_idx = tiling.slice_idx
        prefix_in = f"tile_{slice_idx}_"
        removed = 0
        keys_to_remove = [
            k
            for k in state.tensor_cache
            if k.startswith(prefix_in) and (k.endswith("_in") or k.endswith("_out"))
        ]
        for k in keys_to_remove:
            del state.tensor_cache[k]
            removed += 1
        if removed:
            logging.info(
                f"Cleaned {removed} tile cache entries for slice_idx={slice_idx}"
            )

    def _on_complete(self, state: RunState) -> None:
        """Handle run completion."""
        elapsed = time.perf_counter() - state.start_time
        success = len(state.failed_slices) == 0
        logging.info(
            f"Run {state.run_uid} complete. "
            f"Completed: {len(state.completed_slices)}, Failed: {len(state.failed_slices)}, "
            f"Time: {elapsed:.2f}s"
        )
        if self._on_run_complete:
            self._on_run_complete(state.run_uid, success)


# Backwards compatibility aliases
IncrementalSliceRequest = WorkItem
IncrementalTileRequest = WorkItem
IncrementalRunStatus = RunState
