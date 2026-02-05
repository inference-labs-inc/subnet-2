import atexit
import json
import os
import random
import shutil
import tempfile
import threading
from contextlib import contextmanager
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Callable, Iterator

from bittensor import logging
from deployment_layer.circuit_store import circuit_store
from dsperse.src.analyzers.schema import ExecutionInfo, ExecutionMethod, RunMetadata
import time

from dsperse.src.backends.ezkl import EZKL
from dsperse.src.backends.jstprove import JSTprove
from dsperse.src.run.runner import Runner
from dsperse.src.run.utils.runner_utils import RunnerUtils
from dsperse.src.verify.verifier import Verifier
from dsperse.src.slice.utils.converter import Converter
from execution_layer.circuit import Circuit, CircuitType, ProofSystem

import cli_parser
from _validator.models.dslice_request import DSliceQueuedProofRequest
from _validator.models.request_type import RequestType


SLICE_CACHE_TTL_SECONDS = 300
SLICE_CACHE_MAX_ENTRIES = 32


@dataclass
class CachedSlice:
    path: Path
    accessed_at: float
    in_use: int = 0


class SliceCache:
    def __init__(
        self,
        ttl_seconds: float = SLICE_CACHE_TTL_SECONDS,
        max_entries: int = SLICE_CACHE_MAX_ENTRIES,
    ):
        self._ttl = ttl_seconds
        self._max_entries = max_entries
        self._cache: dict[str, CachedSlice] = {}
        self._meta_lock = threading.Lock()
        self._slice_locks: dict[str, threading.Lock] = {}
        self._cache_dir = Path(tempfile.mkdtemp(prefix="dslice_cache_"))
        atexit.register(self._cleanup_all)

    def _cache_key(self, circuit_id: str, slice_num: str) -> str:
        return f"{circuit_id}:{slice_num}"

    def _get_slice_lock(self, key: str) -> threading.Lock:
        with self._meta_lock:
            if key not in self._slice_locks:
                self._slice_locks[key] = threading.Lock()
            return self._slice_locks[key]

    @contextmanager
    def get_slice_path(
        self, circuit_id: str, base_slice_num: str, dslice_file: Path
    ) -> Iterator[Path]:
        key = self._cache_key(circuit_id, base_slice_num)
        now = time.time()
        path: Path | None = None

        with self._meta_lock:
            self._evict_expired(now)
            if key in self._cache:
                entry = self._cache[key]
                entry.accessed_at = now
                entry.in_use += 1
                path = entry.path

        if path is not None:
            try:
                yield path
            finally:
                with self._meta_lock:
                    if key in self._cache:
                        self._cache[key].in_use -= 1
            return

        slice_lock = self._get_slice_lock(key)
        with slice_lock:
            with self._meta_lock:
                if key in self._cache:
                    entry = self._cache[key]
                    entry.accessed_at = now
                    entry.in_use += 1
                    path = entry.path

            if path is not None:
                try:
                    yield path
                finally:
                    with self._meta_lock:
                        if key in self._cache:
                            self._cache[key].in_use -= 1
                return

            with self._meta_lock:
                self._evict_lru_if_full()

            extract_dir = self._cache_dir / circuit_id / f"slice_{base_slice_num}"
            extract_dir.parent.mkdir(parents=True, exist_ok=True)

            if extract_dir.exists():
                shutil.rmtree(extract_dir)

            Converter.convert(
                path=str(dslice_file),
                output_type="dirs",
                output_path=str(extract_dir),
                cleanup=False,
            )

            with self._meta_lock:
                self._cache[key] = CachedSlice(
                    path=extract_dir, accessed_at=now, in_use=1
                )
            logging.debug(
                f"Extracted dslice {dslice_file.name} to cache: {extract_dir}"
            )
            try:
                yield extract_dir
            finally:
                with self._meta_lock:
                    if key in self._cache:
                        self._cache[key].in_use -= 1

    def _evict_expired(self, now: float):
        expired = [
            k
            for k, v in self._cache.items()
            if now - v.accessed_at > self._ttl and v.in_use == 0
        ]
        for key in expired:
            self._remove_entry(key)

    def _evict_lru_if_full(self):
        while len(self._cache) >= self._max_entries:
            evictable = [k for k, v in self._cache.items() if v.in_use == 0]
            if not evictable:
                break
            lru_key = min(evictable, key=lambda k: self._cache[k].accessed_at)
            self._remove_entry(lru_key)

    def _remove_entry(self, key: str):
        entry = self._cache.pop(key, None)
        self._slice_locks.pop(key, None)
        if entry and entry.path.exists():
            shutil.rmtree(entry.path, ignore_errors=True)
        logging.debug(f"Evicted slice from cache: {key}")

    def _cleanup_all(self):
        with self._meta_lock:
            if self._cache_dir.exists():
                shutil.rmtree(self._cache_dir, ignore_errors=True)
            self._cache.clear()
            self._slice_locks.clear()


_slice_cache = SliceCache()


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

    @property
    def is_complete(self) -> bool:
        return len(self.pending) == 0

    @property
    def all_successful(self) -> bool:
        return self.is_complete and len(self.failed) == 0


class DSperseManager:
    def __init__(self):
        self.circuits: list[Circuit] = [
            circuit
            for circuit in circuit_store.circuits.values()
            if circuit.metadata.type == CircuitType.DSPERSE_PROOF_GENERATION
        ]
        self.runs: dict[str, DsperseRun] = {}
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
        logging.info(
            f"Starting DSperse run for circuit {circuit.metadata.name}. Run UID: {run_uid}"
        )

        run_dir = Path(tempfile.mkdtemp(prefix=f"dsperse_run_{run_uid}_"))

        input_json_path = run_dir / "input.json"
        if inputs is None:
            inputs = circuit.input_handler(RequestType.BENCHMARK).generate()
        with open(input_json_path, "w") as f:
            json.dump(inputs, f)

        runner = Runner(run_dir=run_dir, threads=os.cpu_count() or 4, batch=True)
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
            slice_results, actual_run_dir, circuit.id, runner.run_metadata
        )

        dsperse_run = DsperseRun(
            run_uid=run_uid,
            circuit_id=circuit.id,
            run_dir=run_dir,
            slices={s.slice_num: s for s in slice_data_list},
            pending={s.slice_num for s in slice_data_list},
            callback=callback,
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
        return run_uid, requests

    def generate_dslice_requests(self) -> list[DSliceQueuedProofRequest]:
        if not self.circuits:
            return []
        circuit = random.choice(self.circuits)
        _, requests = self.start_run(circuit)
        return requests

    def on_slice_result(self, run_uid: str, slice_num: str, success: bool) -> bool:
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

        if run.is_complete:
            logging.info(
                f"Run {run_uid} complete. "
                f"Completed: {len(run.completed)}, Failed: {len(run.failed)}"
            )
            if run.callback:
                try:
                    run.callback(run)
                except Exception as e:
                    logging.error(f"Run callback failed: {e}")
            self.cleanup_run(run_uid)

        return run.is_complete

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
                        raise ValueError(
                            f"Tile {tile_idx} of slice {slice_num} missing input/output files"
                        )

                    dslice_data_list.append(
                        DSliceData(
                            slice_num=f"{base_slice_num}_tile_{tile_idx}",
                            input_file=tile_input,
                            output_file=tile_output,
                            witness_file=tile_run_dir / "output_witness.bin",
                            circuit_id=circuit_id,
                            proof_system=DSperseManager._method_to_proof_system(
                                tile_result.method
                            ),
                        )
                    )
            else:
                slice_input = slice_run_dir / "input.json"
                slice_output = slice_run_dir / "output.json"

                if not slice_input.exists() or not slice_output.exists():
                    logging.warning(f"Slice {slice_num} missing input/output files")
                    continue

                dslice_data_list.append(
                    DSliceData(
                        slice_num=base_slice_num,
                        input_file=slice_input,
                        output_file=slice_output,
                        witness_file=slice_run_dir / "output_witness.bin",
                        circuit_id=circuit_id,
                        proof_system=DSperseManager._method_to_proof_system(method),
                    )
                )

        logging.info(f"Generated {len(dslice_data_list)} DSlice requests")
        return dslice_data_list

    @staticmethod
    def _method_to_proof_system(method: str | None) -> ProofSystem:
        if not method:
            return ProofSystem.JSTPROVE
        method_lower = str(method).lower()
        if "ezkl" in method_lower:
            return ProofSystem.EZKL
        if "jstprove" in method_lower or "jst" in method_lower:
            return ProofSystem.JSTPROVE
        logging.warning(f"Unknown proof method '{method}', defaulting to JSTPROVE")
        return ProofSystem.JSTPROVE

    def prove_slice(
        self,
        circuit_id: str,
        slice_num: str,
        inputs: dict,
        outputs: dict,
        proof_system: ProofSystem,
    ) -> dict:
        circuit = self._get_circuit_by_id(circuit_id)
        base_slice_num, tile_idx = self._parse_slice_num(slice_num)
        result = {
            "circuit_id": circuit_id,
            "slice_num": slice_num,
            "success": False,
            "proof_generation_time": None,
            "proof": None,
        }

        with self._get_slice_dir(circuit, base_slice_num) as model_dir:
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
                    )

                slice_copy = tmp_path / "slices"
                shutil.copytree(model_dir, slice_copy)
                runner = Runner(
                    run_dir=tmp_path, threads=os.cpu_count() or 4, batch=True
                )
                runner.run(input_json_path=input_file, slice_path=str(slice_copy))
                run_dir = runner.last_run_dir
                logging.info(
                    f"Runner completed for slice_{slice_num}, run_dir: {run_dir}"
                )

                if runner.run_metadata:
                    metadata_path = run_dir / "metadata.json"
                    with open(metadata_path, "w") as f:
                        json.dump(runner.run_metadata.to_dict(), f)

                prove_start = time.time()

                if proof_system == ProofSystem.JSTPROVE:
                    jst_model_path = (
                        model_dir
                        / "payload"
                        / "jstprove"
                        / f"slice_{base_slice_num}_circuit.txt"
                    )
                    success, proof_data = self._jstprove_witness_and_prove(
                        jst_model_path,
                        input_file,
                        output_file,
                        tmp_path,
                        f"slice {slice_num}",
                    )
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

                    ezkl_circuit = RunnerUtils.resolve_relative_path(
                        slice_meta.ezkl_circuit_path or slice_meta.circuit_path,
                        model_dir,
                    )
                    ezkl_pk = RunnerUtils.resolve_relative_path(
                        slice_meta.ezkl_pk_path or slice_meta.pk_path, model_dir
                    )
                    ezkl_settings = RunnerUtils.resolve_relative_path(
                        slice_meta.ezkl_settings_path or slice_meta.settings_path,
                        model_dir,
                    )
                    witness_path = run_dir / slice_id / "output.json"
                    proof_path = tmp_path / "proof.json"

                    ezkl_runner = EZKL()
                    success, proof_file = ezkl_runner.prove(
                        witness_path=str(witness_path),
                        model_path=str(ezkl_circuit),
                        proof_path=str(proof_path),
                        pk_path=str(ezkl_pk),
                        settings_path=ezkl_settings,
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
    ) -> tuple[bool, str | None]:
        jstprover = JSTprove()
        success, res = jstprover.generate_witness(
            input_file=input_file,
            model_path=circuit_path,
            output_file=output_file,
        )
        if not success:
            logging.error(f"Failed to generate witness for {label}: {res}")
            return False, None

        witness_path = tmp_path / "output_witness.bin"
        proof_path = tmp_path / "proof.bin"
        success, _ = jstprover.prove(
            witness_path=str(witness_path),
            circuit_path=str(circuit_path),
            proof_path=str(proof_path),
        )

        proof_data = None
        if success and proof_path.exists():
            with open(proof_path, "rb") as pf:
                proof_data = pf.read().hex()
        return success, proof_data

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
    ) -> dict:
        prove_start = time.time()
        slice_num = f"{base_slice_num}_tile_{tile_idx}"

        if proof_system == ProofSystem.JSTPROVE:
            jst_tile_circuit = (
                model_dir / "payload" / "jstprove" / "tiles" / "tile_circuit.txt"
            )
            if not jst_tile_circuit.exists():
                logging.error(f"Tile JSTprove circuit not found: {jst_tile_circuit}")
                return result

            success, proof_data = self._jstprove_witness_and_prove(
                jst_tile_circuit, input_file, output_file, tmp_path, f"tile {slice_num}"
            )
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

    @contextmanager
    def _get_slice_dir(self, circuit: Circuit, base_slice_num: str) -> Iterator[Path]:
        base_path = Path(circuit.paths.base_path)
        extracted_dir = base_path / f"slice_{base_slice_num}"
        if extracted_dir.exists():
            yield extracted_dir
            return

        dslice_file = base_path / f"slice_{base_slice_num}.dslice"
        if dslice_file.exists():
            with _slice_cache.get_slice_path(
                circuit.id, base_slice_num, dslice_file
            ) as path:
                yield path
            return

        raise FileNotFoundError(
            f"No slice found for {circuit.id} slice {base_slice_num}: "
            f"checked {extracted_dir} and {dslice_file}"
        )

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

        with self._get_slice_dir(circuit, base_slice_num) as slice_dir:
            if proof_system == ProofSystem.JSTPROVE:
                if tile_idx is not None:
                    circuit_path = (
                        slice_dir
                        / "payload"
                        / "jstprove"
                        / "tiles"
                        / "tile_circuit.txt"
                    )
                else:
                    circuit_path = (
                        slice_dir
                        / "payload"
                        / "jstprove"
                        / f"slice_{base_slice_num}_circuit.txt"
                    )
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
                    model_path=slice_dir,
                    backend=proof_system.value.lower() if proof_system else None,
                )
                _, verification_execution = self._parse_dsperse_result(
                    result, "verification"
                )
                success = verification_execution.get("success", False)

            slice_data.success = success
            return success

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

    @classmethod
    def extract_dslices(cls, model_path: Path | str) -> None:
        """No-op for backwards compatibility. DSlices are now extracted on-demand."""
        pass

    @classmethod
    def validate_dslices(cls, model_path: Path | str) -> list[Path]:
        model_path = Path(model_path)
        dslice_files = list(model_path.glob("slice_*.dslice"))
        extracted_dirs = list(model_path.glob("slice_*"))
        extracted_dirs = [d for d in extracted_dirs if d.is_dir()]
        if dslice_files:
            logging.debug(f"Found {len(dslice_files)} dslice files in {model_path}")
        if extracted_dirs:
            logging.debug(
                f"Found {len(extracted_dirs)} extracted slice dirs in {model_path}"
            )
        return dslice_files + extracted_dirs
