"""
IncrementalRunner for distributed slice execution in the validator.

This module provides an IncrementalRunner that orchestrates distributed execution
of DSperse models across miners. Unlike the standard DSperseManager which pre-computes
all outputs locally, IncrementalRunner sends slices to miners sequentially, where
each slice's verified output becomes the input for the next slice.
"""

import secrets
import time
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any, Callable, Optional, Union

from bittensor import logging

from dsperse.src.run.incremental_runner import (
    IncrementalRunner as DsperseIncrementalRunner,
    IncrementalRunState,
    SliceTask,
    SliceResult,
    TileTask,
    TileResult,
)
from dsperse.src.analyzers.schema import Backend
from execution_layer.circuit import Circuit, ProofSystem
from _validator.models.dslice_request import DSliceQueuedProofRequest
from _validator.models.request_type import RequestType


@dataclass
class IncrementalSliceRequest:
    """A slice request ready to be sent to a miner."""

    slice_id: str
    slice_index: int
    inputs: dict
    proof_system: ProofSystem
    circuit: Circuit
    run_uid: str
    use_circuit: bool
    is_tiled: bool = False
    tile_count: int = 0


@dataclass
class IncrementalTileRequest:
    """A tile request ready to be sent to a miner."""

    task_id: str
    slice_id: str
    tile_idx: int
    inputs: dict
    proof_system: ProofSystem
    circuit: Circuit
    run_uid: str
    use_circuit: bool


@dataclass
class IncrementalRunStatus:
    """Status of an incremental run."""

    run_uid: str
    circuit_id: str
    circuit_name: str
    total_slices: int
    current_slice: Optional[str]
    completed_slices: list[str] = field(default_factory=list)
    failed_slices: list[str] = field(default_factory=list)
    pending_slice: Optional[str] = None
    pending_tiles: dict[int, bool] = field(default_factory=dict)
    failed_tile_slices: set[str] = field(default_factory=set)
    start_time: float = 0.0

    @property
    def is_complete(self) -> bool:
        return (
            self.current_slice is None
            and not self.pending_slice
            and not self.pending_tiles
        )

    @property
    def progress_percent(self) -> float:
        if self.total_slices == 0:
            return 0.0
        return (
            (len(self.completed_slices) + len(self.failed_slices))
            / self.total_slices
            * 100
        )


class IncrementalRunner:
    """
    Orchestrates distributed slice execution across miners.

    This runner sends slices to miners one at a time, waits for verified outputs,
    and chains them to subsequent slices. This enables true distributed model
    execution where the validator doesn't pre-compute outputs.

    Usage:
        runner = IncrementalRunner()
        run_uid = runner.start_run(circuit, inputs)

        # Get next slice to send
        slice_req = runner.get_next_slice(run_uid)
        if slice_req:
            # Send to miner and get result
            result = send_to_miner(slice_req)
            runner.apply_slice_result(run_uid, result)

        # Check completion
        if runner.is_complete(run_uid):
            final_output = runner.get_final_output(run_uid)
    """

    def __init__(self, on_run_complete: Optional[Callable[[str, bool], None]] = None):
        """
        Initialize the IncrementalRunner.

        Args:
            on_run_complete: Optional callback when a run completes.
                            Signature: (run_uid: str, success: bool) -> None
        """
        self._dsperse_runner = DsperseIncrementalRunner(verify_proofs=False)
        self._runs: dict[
            str, tuple[IncrementalRunState, IncrementalRunStatus, Circuit]
        ] = {}
        self._on_run_complete = on_run_complete

    def start_run(
        self,
        circuit: Circuit,
        inputs: Optional[dict] = None,
    ) -> str:
        """
        Start a new incremental run.

        Args:
            circuit: The DSperse circuit to execute
            inputs: Model inputs (generated if not provided)

        Returns:
            Run UID for tracking this run
        """
        run_uid = f"{datetime.now().strftime('%Y%m%d%H%M%S%f')}-{secrets.token_hex(8)}"
        logging.info(
            f"Starting incremental run for circuit {circuit.metadata.name}. Run UID: {run_uid}"
        )

        if inputs is None:
            inputs = circuit.input_handler(RequestType.BENCHMARK).generate()

        state = self._dsperse_runner.initialize(
            slice_path=circuit.paths.base_path,
            input_data=inputs,
        )

        total_slices = len(state.run_metadata.execution_chain.nodes)
        status = IncrementalRunStatus(
            run_uid=run_uid,
            circuit_id=circuit.id,
            circuit_name=circuit.metadata.name,
            total_slices=total_slices,
            current_slice=state.current_slice_id,
            start_time=time.perf_counter(),
        )

        self._runs[run_uid] = (state, status, circuit)
        logging.info(
            f"Incremental run {run_uid} initialized with {total_slices} slices"
        )

        expected_backend = (
            Backend.JSTPROVE
            if circuit.metadata.proof_system == ProofSystem.JSTPROVE
            else Backend.EZKL
        )
        for slice_id, node in state.run_metadata.execution_chain.nodes.items():
            if node.backend != expected_backend:
                logging.warning(
                    f"Overriding node {slice_id} backend from {node.backend!r} to {expected_backend!r} "
                    f"(circuit proof_system={circuit.metadata.proof_system})"
                )
                node.backend = expected_backend
            logging.info(
                f"[TILE DEBUG] Node {slice_id}: backend={node.backend!r}, use_circuit={node.use_circuit}"
            )

        return run_uid

    def get_next_slice(self, run_uid: str) -> Optional[IncrementalSliceRequest]:
        """
        Get the next slice that needs execution (non-tiled only).

        For tiled slices, use get_tile_requests() instead.

        Args:
            run_uid: The run identifier

        Returns:
            IncrementalSliceRequest if there's a non-tiled slice to execute, None otherwise
        """
        if run_uid not in self._runs:
            logging.warning(f"Run {run_uid} not found")
            return None

        state, status, circuit = self._runs[run_uid]

        if status.pending_slice or status.pending_tiles:
            logging.debug(f"Run {run_uid} has pending work")
            return None

        if state.current_slice_id is None:
            return None

        for task in self._dsperse_runner.iter_tasks(state):
            if isinstance(task, TileTask):
                return None

            if not task.use_circuit:
                result = self._dsperse_runner.execute_onnx_slice(state, task)
                if result is None or not result.success:
                    error_msg = (
                        result.error if result else "execute_onnx_slice returned None"
                    )
                    logging.error(
                        f"ONNX-only slice {task.slice_id} execution failed: {error_msg}"
                    )
                    failed_result = SliceResult(
                        slice_id=task.slice_id,
                        success=False,
                        error=error_msg,
                    )
                    self._dsperse_runner.apply_result(state, failed_result)
                    status.failed_slices.append(task.slice_id)
                    status.current_slice = state.current_slice_id
                    continue
                self._dsperse_runner.apply_result(state, result)
                status.completed_slices.append(task.slice_id)
                status.current_slice = state.current_slice_id
                logging.debug(f"Executed ONNX-only slice {task.slice_id} locally")
                continue

            proof_system = self._determine_proof_system(task)
            if proof_system is None:
                failed_result = SliceResult(
                    slice_id=task.slice_id,
                    success=False,
                    error=f"Unknown backend '{task.backend}'",
                )
                self._dsperse_runner.apply_result(state, failed_result)
                status.failed_slices.append(task.slice_id)
                status.current_slice = state.current_slice_id
                continue

            request = IncrementalSliceRequest(
                slice_id=task.slice_id,
                slice_index=task.slice_index,
                inputs=task.inputs,
                proof_system=proof_system,
                circuit=circuit,
                run_uid=run_uid,
                use_circuit=task.use_circuit,
                is_tiled=task.is_tiled,
                tile_count=task.tile_count,
            )

            status.pending_slice = task.slice_id
            return request

        return None

    def get_tile_requests(self, run_uid: str) -> list[IncrementalTileRequest]:
        """
        Get all tile requests for the current tiled slice.

        Returns tile requests that can be executed in parallel.

        Args:
            run_uid: The run identifier

        Returns:
            List of IncrementalTileRequest objects, empty if no tiles pending
        """
        if run_uid not in self._runs:
            logging.warning(f"Run {run_uid} not found")
            return []

        state, status, circuit = self._runs[run_uid]

        if status.pending_slice or status.pending_tiles:
            logging.debug(f"Run {run_uid} has pending work")
            return []

        pending = state.pending_tiled_slice
        if pending and not pending.is_complete:
            return self._generate_tile_requests_from_pending(
                state, status, circuit, run_uid, pending
            )

        if state.current_slice_id is None:
            return []

        tile_requests = []
        for task in self._dsperse_runner.iter_tasks(state):
            if isinstance(task, TileTask):
                logging.debug(
                    f"[TILE DEBUG] get_tile_requests: TileTask found, task.backend={task.backend!r}, "
                    f"slice_id={task.slice_id}, tile_idx={task.tile_idx}"
                )
                proof_system = self._determine_proof_system(task)
                if proof_system is None:
                    continue
                logging.debug(
                    f"[TILE DEBUG] get_tile_requests: creating request with proof_system={proof_system}"
                )
                request = IncrementalTileRequest(
                    task_id=task.task_id,
                    slice_id=task.slice_id,
                    tile_idx=task.tile_idx,
                    inputs=task.inputs,
                    proof_system=proof_system,
                    circuit=circuit,
                    run_uid=run_uid,
                    use_circuit=task.use_circuit,
                )
                tile_requests.append(request)
                status.pending_tiles[task.tile_idx] = True

        if tile_requests:
            logging.info(
                f"Generated {len(tile_requests)} tile requests for slice "
                f"{tile_requests[0].slice_id}"
            )

        return tile_requests

    def _generate_tile_requests_from_pending(
        self,
        state: IncrementalRunState,
        status: IncrementalRunStatus,
        circuit: Circuit,
        run_uid: str,
        pending,
    ) -> list[IncrementalTileRequest]:
        """Generate tile requests from pending_tiled_slice state."""
        tile_requests = []
        nodes = state.run_metadata.execution_chain.nodes
        node = nodes.get(pending.slice_id)

        logging.info(
            f"[TILE DEBUG] _generate_tile_requests_from_pending: pending.slice_id={pending.slice_id}, "
            f"node.backend={getattr(node, 'backend', 'MISSING')!r}"
        )

        if not node:
            logging.error(f"Node not found for pending tiled slice {pending.slice_id}")
            return []

        tiling = pending.tiling_info
        if not tiling:
            logging.error(f"Tiling info not found for pending slice {pending.slice_id}")
            return []

        slice_idx = tiling.slice_idx

        for tile_idx in range(pending.total_tiles):
            if tile_idx in pending.completed_tiles or tile_idx in pending.failed_tiles:
                continue

            cache_name = f"tile_{slice_idx}_{tile_idx}_in"
            tile_tensor = state.tensor_cache.get(cache_name)

            if tile_tensor is None:
                logging.error(
                    f"Tile input {cache_name} not found in cache for slice {pending.slice_id}"
                )
                pending.failed_tiles.append(tile_idx)
                continue

            tile_inputs = {"input_data": tile_tensor.tolist()}
            backend = node.backend.lower() if node.backend else ""

            logging.info(
                f"[TILE DEBUG] tile {tile_idx}: node.backend={node.backend!r}, backend_lower={backend!r}"
            )

            if "jstprove" in backend or "jst" in backend:
                proof_system = ProofSystem.JSTPROVE
                logging.info(f"[TILE DEBUG] tile {tile_idx}: set JSTPROVE")
            elif "ezkl" in backend:
                proof_system = ProofSystem.EZKL
                logging.info(f"[TILE DEBUG] tile {tile_idx}: set EZKL")
            else:
                logging.error(
                    f"Unknown backend '{node.backend}' for tile {tile_idx} of slice {pending.slice_id}"
                )
                pending.failed_tiles.append(tile_idx)
                continue

            request = IncrementalTileRequest(
                task_id=f"{pending.slice_id}_tile_{tile_idx}",
                slice_id=pending.slice_id,
                tile_idx=tile_idx,
                inputs=tile_inputs,
                proof_system=proof_system,
                circuit=circuit,
                run_uid=run_uid,
                use_circuit=node.use_circuit,
            )
            tile_requests.append(request)
            status.pending_tiles[tile_idx] = True

        if tile_requests:
            logging.info(
                f"Generated {len(tile_requests)} tile requests from pending state for slice "
                f"{pending.slice_id}"
            )

        return tile_requests

    def create_queued_request(
        self, slice_req: IncrementalSliceRequest
    ) -> DSliceQueuedProofRequest:
        """
        Create a DSliceQueuedProofRequest from an IncrementalSliceRequest.

        Args:
            slice_req: The slice request

        Returns:
            DSliceQueuedProofRequest ready to be queued for a miner
        """
        return DSliceQueuedProofRequest(
            circuit=slice_req.circuit,
            inputs=slice_req.inputs,
            outputs=None,
            slice_num=slice_req.slice_id.replace("slice_", ""),
            run_uid=slice_req.run_uid,
            proof_system=slice_req.proof_system,
            compute_outputs=True,
        )

    def create_tile_queued_request(
        self, tile_req: IncrementalTileRequest
    ) -> DSliceQueuedProofRequest:
        """
        Create a DSliceQueuedProofRequest from an IncrementalTileRequest.

        Args:
            tile_req: The tile request

        Returns:
            DSliceQueuedProofRequest ready to be queued for a miner
        """
        base_slice_num = tile_req.slice_id.replace("slice_", "")
        tile_slice_num = f"{base_slice_num}_tile_{tile_req.tile_idx}"

        logging.debug(
            f"[TILE DEBUG] create_tile_queued_request: tile_req.proof_system={tile_req.proof_system}, "
            f"slice_id={tile_req.slice_id}, tile_idx={tile_req.tile_idx}, circuit={tile_req.circuit}"
        )

        return DSliceQueuedProofRequest(
            circuit=tile_req.circuit,
            inputs=tile_req.inputs,
            outputs=None,
            slice_num=tile_slice_num,
            run_uid=tile_req.run_uid,
            proof_system=tile_req.proof_system,
            compute_outputs=True,
            is_tile=True,
            tile_idx=tile_req.tile_idx,
            task_id=tile_req.task_id,
        )

    def apply_slice_result(
        self,
        run_uid: str,
        slice_id: str,
        success: bool,
        computed_outputs: Optional[dict] = None,
        proof: Optional[Any] = None,
        error: Optional[str] = None,
    ) -> bool:
        """
        Apply the result of a slice execution.

        Args:
            run_uid: The run identifier
            slice_id: The slice that was executed
            success: Whether execution succeeded
            computed_outputs: The outputs computed by the miner
            proof: The proof generated by the miner
            error: Error message if failed

        Returns:
            True if this was the final slice (run is complete)
        """
        if run_uid not in self._runs:
            logging.warning(f"Run {run_uid} not found")
            return False

        state, status, circuit = self._runs[run_uid]

        if status.pending_slice != slice_id:
            logging.warning(
                f"Slice {slice_id} doesn't match pending slice {status.pending_slice}"
            )
            return False

        status.pending_slice = None

        slice_result = SliceResult(
            slice_id=slice_id,
            success=success,
            outputs=computed_outputs,
            error=error,
            proof=proof,
        )

        applied = self._dsperse_runner.apply_result(state, slice_result)

        if applied:
            status.completed_slices.append(slice_id)
        else:
            status.failed_slices.append(slice_id)

        status.current_slice = state.current_slice_id

        is_complete = self._dsperse_runner.is_complete(state)
        if is_complete:
            total_time = time.perf_counter() - status.start_time
            all_success = len(status.failed_slices) == 0
            logging.info(
                f"Incremental run {run_uid} complete. "
                f"Completed: {len(status.completed_slices)}, Failed: {len(status.failed_slices)}, "
                f"Time: {total_time:.2f}s"
            )
            if self._on_run_complete:
                self._on_run_complete(run_uid, all_success)

        return is_complete

    def apply_tile_result(
        self,
        run_uid: str,
        task_id: str,
        slice_id: str,
        tile_idx: int,
        success: bool,
        computed_outputs: Optional[dict] = None,
        proof: Optional[bytes] = None,
        witness: Optional[bytes] = None,
        error: Optional[str] = None,
    ) -> bool:
        """
        Apply the result of a tile execution.

        Args:
            run_uid: The run identifier
            task_id: The tile task identifier
            slice_id: The parent slice identifier
            tile_idx: The tile index
            success: Whether execution succeeded
            computed_outputs: The outputs computed by the miner
            proof: The proof bytes
            witness: The witness bytes
            error: Error message if failed

        Returns:
            True if this completes the tiled slice (ready for next slice)
        """
        if run_uid not in self._runs:
            logging.warning(f"Run {run_uid} not found")
            return False

        state, status, circuit = self._runs[run_uid]

        if tile_idx not in status.pending_tiles:
            logging.warning(f"Tile {tile_idx} not in pending tiles")
            return False

        del status.pending_tiles[tile_idx]

        tile_result = TileResult(
            task_id=task_id,
            slice_id=slice_id,
            tile_idx=tile_idx,
            success=success,
            outputs=computed_outputs,
            error=error,
            proof=proof,
            witness=witness,
        )

        applied = self._dsperse_runner.apply_tile_result(state, tile_result)

        if not applied or not success:
            logging.warning(f"Failed to apply tile result for {task_id}")
            status.failed_tile_slices.add(slice_id)

        if not status.pending_tiles:
            status.current_slice = state.current_slice_id
            if state.pending_tiled_slice is None:
                slice_completed = state.current_slice_id != slice_id
                if slice_completed:
                    if slice_id in status.failed_tile_slices:
                        status.failed_slices.append(slice_id)
                        status.failed_tile_slices.discard(slice_id)
                        logging.info(
                            f"Tiled slice {slice_id} failed (one or more tiles failed)"
                        )
                    else:
                        status.completed_slices.append(slice_id)
                        logging.info(f"Tiled slice {slice_id} completed")

        is_complete = self._dsperse_runner.is_complete(state)
        if is_complete:
            total_time = time.perf_counter() - status.start_time
            all_success = len(status.failed_slices) == 0
            logging.info(
                f"Incremental run {run_uid} complete. "
                f"Completed: {len(status.completed_slices)}, Failed: {len(status.failed_slices)}, "
                f"Time: {total_time:.2f}s"
            )
            if self._on_run_complete:
                self._on_run_complete(run_uid, all_success)

        return not status.pending_tiles

    def get_run_status(self, run_uid: str) -> Optional[dict]:
        """Get status of a run."""
        if run_uid not in self._runs:
            return None

        state, status, _ = self._runs[run_uid]

        return {
            "run_uid": run_uid,
            "circuit_id": status.circuit_id,
            "circuit_name": status.circuit_name,
            "total_slices": status.total_slices,
            "completed": len(status.completed_slices),
            "failed": len(status.failed_slices),
            "pending_slice": status.pending_slice,
            "current_slice": status.current_slice,
            "is_complete": status.is_complete,
            "progress_percent": status.progress_percent,
            "elapsed_time": time.perf_counter() - status.start_time,
        }

    def get_final_output(self, run_uid: str) -> Optional[Any]:
        """Get the final output tensor after run completion."""
        if run_uid not in self._runs:
            return None

        state, status, _ = self._runs[run_uid]
        if not status.is_complete:
            return None

        output = self._dsperse_runner.get_final_output(state)
        if output is not None and hasattr(output, "tolist"):
            return output.tolist()
        return output

    def is_complete(self, run_uid: str) -> bool:
        """Check if a run is complete."""
        if run_uid not in self._runs:
            logging.debug(f"is_complete: run_uid {run_uid} not found in runs")
            return False
        state, status, _ = self._runs[run_uid]
        return status.is_complete

    def cleanup_run(self, run_uid: str) -> None:
        """Clean up a completed run."""
        if run_uid in self._runs:
            del self._runs[run_uid]
            logging.debug(f"Cleaned up incremental run {run_uid}")

    def _determine_proof_system(
        self, task: Union[SliceTask, TileTask]
    ) -> ProofSystem | None:
        """Determine the proof system to use for a slice or tile."""
        backend = task.backend.lower() if task.backend else ""
        logging.debug(
            f"[TILE DEBUG] _determine_proof_system: task.backend={task.backend!r}, "
            f"backend_lower={backend!r}, slice_id={getattr(task, 'slice_id', 'unknown')}"
        )
        if "jstprove" in backend or "jst" in backend:
            logging.debug("[TILE DEBUG] _determine_proof_system: returning JSTPROVE")
            return ProofSystem.JSTPROVE
        if "ezkl" in backend:
            logging.debug("[TILE DEBUG] _determine_proof_system: returning EZKL")
            return ProofSystem.EZKL
        logging.error(
            f"Unknown backend '{task.backend}' for task {getattr(task, 'slice_id', 'unknown')}"
        )
        return None
