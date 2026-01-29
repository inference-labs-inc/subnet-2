import json
import random
import tempfile
import shutil
from concurrent.futures import ProcessPoolExecutor, as_completed
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Iterable

from bittensor import logging
from deployment_layer.circuit_store import circuit_store
from dsperse.src.analyzers.schema import ExecutionInfo, ExecutionMethod
from dsperse.src.backends.jstprove import JSTprove
from dsperse.src.prove.prover import Prover
from dsperse.src.run.runner import Runner
from dsperse.src.verify.verifier import Verifier
from dsperse.src.slice.utils.converter import Converter
from execution_layer.circuit import Circuit, CircuitType, ProofSystem

import cli_parser
from _validator.models.dslice_request import DSliceQueuedProofRequest
from _validator.models.request_type import RequestType
from utils.pre_flight import SYNC_LOG_PREFIX


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
    frame_idx: int | None = None


@dataclass
class FrameWitnessResult:
    frame_idx: int
    run_uid: str
    slices: list[DSliceData]
    success: bool
    error: str | None = None


@dataclass
class BatchRun:
    batch_id: str
    circuit_id: str
    frame_results: dict[int, FrameWitnessResult] = field(default_factory=dict)
    pending_slices: list[DSliceQueuedProofRequest] = field(default_factory=list)
    failed_slices: list[DSliceQueuedProofRequest] = field(default_factory=list)
    completed_slices: list[str] = field(default_factory=list)


class DSperseManager:
    def __init__(self):
        self.circuits: list[Circuit] = [
            circuit
            for circuit in circuit_store.circuits.values()
            if circuit.metadata.type == CircuitType.DSPERSE_PROOF_GENERATION
        ]
        self.runs = {}

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

    def generate_dslice_requests(self) -> Iterable[DSliceQueuedProofRequest]:
        if not self.circuits:
            return

        circuit = random.choice(self.circuits)
        run_uid = datetime.now().strftime("%Y%m%d%H%M%S%f")
        logging.info(
            f"Generating DSlice requests for circuit {circuit.metadata.name}... Run UID: {run_uid}"
        )

        slices: list[DSliceData] = self.run_dsperse(circuit, run_uid)
        self.runs[run_uid] = slices

        for slice_data in slices:
            with open(slice_data.input_file, "r") as input_file:
                with open(slice_data.output_file, "r") as output_file:
                    yield DSliceQueuedProofRequest(
                        circuit=circuit,
                        inputs=json.load(input_file),
                        outputs=json.load(output_file),
                        slice_num=slice_data.slice_num,
                        run_uid=run_uid,
                        proof_system=slice_data.proof_system,
                    )

    def run_dsperse(
        self,
        circuit: Circuit,
        run_uid: str,
    ) -> list[DSliceData]:
        run_metadata_path = Path(cli_parser.config.dsperse_run_dir) / f"run_{run_uid}"
        run_metadata_path.mkdir(parents=True, exist_ok=True)
        logging.debug(f"Running DSperse model. Run metadata path: {run_metadata_path}")

        input_json_path = run_metadata_path / "input.json"
        with open(input_json_path, "w") as f:
            json.dump(circuit.input_handler(RequestType.BENCHMARK).generate(), f)

        runner = Runner(run_dir=run_metadata_path)
        results = runner.run(
            input_json_path=input_json_path, slice_path=circuit.paths.external_base_path
        )
        run_dir = runner.last_run_dir
        logging.debug(f"DSperse run completed. Results data saved at {run_dir}")

        slice_results: dict[str, ExecutionInfo] = results.get("slice_results", {})

        if not all(info.success for info in slice_results.values()):
            logging.error(
                "DSperse run failed for some slices. Aborting request generation..."
            )
            return []

        return DSperseManager._extract_dslice_data(slice_results, run_dir, circuit.id)

    @staticmethod
    def _extract_dslice_data(
        slice_results: dict[str, ExecutionInfo],
        run_dir: Path,
        circuit_id: str,
        frame_idx: int | None = None,
    ) -> list[DSliceData]:
        dslice_data_list = []

        for slice_num, exec_info in slice_results.items():
            method = str(exec_info.method)
            if method.startswith("onnx"):
                continue

            base_slice_num = slice_num.split("_")[-1]
            slice_run_dir = run_dir / slice_num

            if exec_info.method == ExecutionMethod.TILED:
                logging.info(
                    f"Expanding tiled slice {slice_num} into {len(exec_info.tiles)} tile requests"
                )
                for tile_idx, tile_result in enumerate(exec_info.tiles):
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
                            witness_file=None,
                            circuit_id=circuit_id,
                            proof_system=DSperseManager._method_to_proof_system(
                                tile_result.method
                            ),
                            frame_idx=frame_idx,
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
                        witness_file=None,
                        circuit_id=circuit_id,
                        proof_system=DSperseManager._method_to_proof_system(method),
                        frame_idx=frame_idx,
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
        model_dir = Path(circuit.paths.external_base_path) / f"slice_{base_slice_num}"
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

            runner = Runner(run_dir=tmp_path, threads=16)
            runner.run(input_json_path=input_file, slice_path=model_dir)
            run_dir = runner.last_run_dir
            logging.info(f"Runner completed for slice_{slice_num}, run_dir: {run_dir}")

            if runner.run_metadata:
                metadata_path = run_dir / "metadata.json"
                with open(metadata_path, "w") as f:
                    json.dump(runner.run_metadata.to_dict(), f)

            if proof_system == ProofSystem.JSTPROVE:
                jstprover = JSTprove()
                jst_model_path = (
                    model_dir
                    / "payload"
                    / "jstprove"
                    / f"slice_{base_slice_num}_circuit.txt"
                )
                success, res = jstprover.generate_witness(
                    input_file=input_file,
                    model_path=jst_model_path,
                    output_file=output_file,
                )
                if not success:
                    logging.error(
                        f"Failed to generate witness for slice {slice_num} using JSTprove. Error: {res}"
                    )
                    return result

            prover = Prover()
            proving_result = prover.prove(
                run_path=run_dir,
                model_dir=model_dir,
                output_path=tmp_path,
                backend=proof_system.value.lower(),
            )
            logging.debug(f"Got proof generation result. Result: {proving_result}")

            _, proof_execution = self._parse_dsperse_result(proving_result, "proof")

            success = proof_execution.get("success", False)
            proof_generation_time = proof_execution.get("proof_generation_time", None)
            proof_data = None
            if proof_execution.get("proof_file", None):
                if proof_system == ProofSystem.JSTPROVE:
                    with open(proof_execution["proof_file"], "rb") as proof_file:
                        proof_data = proof_file.read().hex()
                else:
                    with open(proof_execution["proof_file"], "r") as proof_file:
                        proof_data = json.load(proof_file)

            result["success"] = success
            result["proof_generation_time"] = proof_generation_time
            result["proof"] = proof_data
            return result

    def _parse_slice_num(self, slice_num: str) -> tuple[str, int | None]:
        if "_tile_" in slice_num:
            parts = slice_num.split("_tile_")
            return parts[0], int(parts[1])
        return slice_num, None

    def verify_slice_proof(
        self, run_uid: str, slice_num: str, proof: dict | str, proof_system: ProofSystem
    ) -> bool:
        if run_uid not in self.runs:
            raise ValueError(f"Run UID {run_uid} not found.")

        slice_data: DSliceData = next(
            (s for s in self.runs[run_uid] if s.slice_num == slice_num), None
        )
        if slice_data is None:
            raise ValueError(f"Slice data for slice number {slice_num} not found.")
        if slice_data.proof_system != proof_system:
            raise ValueError(
                f"Proof system mismatch for slice {slice_num} in run {run_uid}. "
                f"Expected {slice_data.proof_system}, got {proof_system}."
            )

        circuit = self._get_circuit_by_id(slice_data.circuit_id)
        base_slice_num, _ = self._parse_slice_num(slice_num)

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

        verifier = Verifier()
        result = verifier.verify(
            run_path=slice_data.input_file.parent,
            model_path=Path(circuit.paths.external_base_path)
            / f"slice_{base_slice_num}",
            backend=proof_system.value.lower() if proof_system else None,
        )

        logging.debug(f"Got proof verification result. Result: {result}")

        _, verification_execution = self._parse_dsperse_result(result, "verification")
        success = verification_execution.get("success", False)
        slice_data.success = success
        return success

    def check_run_completion(
        self, run_uid: str, remove_completed: bool = False
    ) -> bool:
        if run_uid not in self.runs:
            raise ValueError(f"Run UID {run_uid} not found.")

        slices: list[DSliceData] = self.runs[run_uid]
        all_verified = all(slice_data.success for slice_data in slices)
        if all_verified and remove_completed:
            self.cleanup_run(run_uid)
        return all_verified

    def cleanup_run(self, run_uid: str):
        if run_uid not in self.runs:
            raise ValueError(f"Cannot cleanup run data. Run UID {run_uid} not found.")
        logging.info(f"Cleaning up run data for run UID {run_uid}...")
        run_path = self.runs[run_uid][0].input_file.parent.parent
        if run_path.exists() and run_path.is_dir():
            shutil.rmtree(run_path)
        del self.runs[run_uid]

    def total_cleanup(self):
        logging.info("Performing total cleanup of all DSperse run data...")
        for run_uid in list(self.runs.keys()):
            self.cleanup_run(run_uid)

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

        if execution_type == "proof":
            proof_file = execution.get("proof_file") or execution.get("proof_path")
            if not proof_file:
                tiles = (
                    execution.get("tiles") or execution.get("tile_proofs_info") or []
                )
                if tiles and len(tiles) > 0:
                    tile_0 = tiles[0]
                    if isinstance(tile_0, dict):
                        proof_file = tile_0.get("proof_path")
                        if tile_0.get("success") is not None:
                            execution["success"] = tile_0["success"]
            if proof_file:
                execution["proof_file"] = proof_file

        return slice_id, execution

    @classmethod
    def extract_dslices(cls, model_path: Path | str) -> None:
        model_path = Path(model_path)
        dslice_files = list(model_path.glob("slice_*.dslice"))
        if not dslice_files:
            return
        logging.debug(SYNC_LOG_PREFIX + f"Extracting DSlices for model {model_path}...")
        for dslice_file in dslice_files:
            extracted_path = dslice_file.with_suffix("")
            if extracted_path.exists():
                shutil.rmtree(extracted_path)
            logging.info(
                SYNC_LOG_PREFIX
                + f"Extracting DSlice file {dslice_file} to {extracted_path}..."
            )
            Converter.convert(
                path=dslice_file,
                output_type="dirs",
                output_path=extracted_path,
                cleanup=True,
            )
            dslice_file.unlink(missing_ok=True)

    def generate_batch_witnesses(
        self,
        circuit: Circuit,
        frame_inputs: list[dict],
        max_workers: int = 8,
    ) -> BatchRun:
        batch_id = datetime.now().strftime("%Y%m%d%H%M%S%f")
        batch_run = BatchRun(batch_id=batch_id, circuit_id=circuit.id)

        logging.info(
            f"Starting batch witness generation for {len(frame_inputs)} frames "
            f"with {max_workers} workers. Batch ID: {batch_id}"
        )

        with ProcessPoolExecutor(max_workers=max_workers) as executor:
            futures = {
                executor.submit(
                    _process_single_frame,
                    frame_idx,
                    frame_input,
                    circuit.id,
                    circuit.paths.external_base_path,
                    batch_id,
                ): frame_idx
                for frame_idx, frame_input in enumerate(frame_inputs)
            }

            for future in as_completed(futures):
                frame_idx = futures[future]
                try:
                    result = future.result()
                    batch_run.frame_results[frame_idx] = result

                    if result.success:
                        logging.info(
                            f"Frame {frame_idx} witness generation complete: "
                            f"{len(result.slices)} slices"
                        )
                    else:
                        logging.warning(
                            f"Frame {frame_idx} witness generation failed: {result.error}"
                        )
                except Exception as e:
                    logging.error(f"Frame {frame_idx} raised exception: {e}")
                    batch_run.frame_results[frame_idx] = FrameWitnessResult(
                        frame_idx=frame_idx,
                        run_uid=f"{batch_id}_{frame_idx}",
                        slices=[],
                        success=False,
                        error=str(e),
                    )

        self._populate_batch_requests(batch_run, circuit)
        return batch_run

    def _populate_batch_requests(self, batch_run: BatchRun, circuit: Circuit) -> None:
        for frame_idx, result in batch_run.frame_results.items():
            if not result.success:
                continue

            self.runs[result.run_uid] = result.slices

            for slice_data in result.slices:
                try:
                    with open(slice_data.input_file, "r") as f:
                        inputs = json.load(f)
                    with open(slice_data.output_file, "r") as f:
                        outputs = json.load(f)

                    request = DSliceQueuedProofRequest(
                        circuit=circuit,
                        inputs=inputs,
                        outputs=outputs,
                        slice_num=slice_data.slice_num,
                        run_uid=result.run_uid,
                        proof_system=slice_data.proof_system,
                    )
                    batch_run.pending_slices.append(request)
                except Exception as e:
                    logging.error(
                        f"Failed to create request for frame {frame_idx} "
                        f"slice {slice_data.slice_num}: {e}"
                    )
                    failed_request = DSliceQueuedProofRequest(
                        circuit=circuit,
                        inputs={},
                        outputs={},
                        slice_num=slice_data.slice_num,
                        run_uid=result.run_uid,
                        proof_system=slice_data.proof_system,
                    )
                    batch_run.failed_slices.append(failed_request)

        logging.info(
            f"Batch {batch_run.batch_id}: {len(batch_run.pending_slices)} "
            f"proof requests ready for distribution"
        )

    def mark_slice_complete(
        self, batch_run: BatchRun, run_uid: str, slice_num: str
    ) -> None:
        slice_key = f"{run_uid}:{slice_num}"
        if slice_key not in batch_run.completed_slices:
            batch_run.completed_slices.append(slice_key)

    def get_batch_progress(self, batch_run: BatchRun) -> dict:
        total_frames = len(batch_run.frame_results)
        successful_frames = sum(
            1 for r in batch_run.frame_results.values() if r.success
        )
        total_slices = sum(
            len(r.slices) for r in batch_run.frame_results.values() if r.success
        )

        return {
            "batch_id": batch_run.batch_id,
            "total_frames": total_frames,
            "successful_frames": successful_frames,
            "failed_frames": total_frames - successful_frames,
            "total_slices": total_slices,
            "pending_slices": len(batch_run.pending_slices),
            "completed_slices": len(batch_run.completed_slices),
            "failed_slices": len(batch_run.failed_slices),
            "progress_percent": (
                len(batch_run.completed_slices) / total_slices * 100
                if total_slices > 0
                else 0
            ),
        }


def _process_single_frame(
    frame_idx: int,
    frame_input: dict,
    circuit_id: str,
    slice_path: str,
    batch_id: str,
) -> FrameWitnessResult:
    from dsperse.src.analyzers.schema import ExecutionInfo

    run_uid = f"{batch_id}_{frame_idx}"
    run_dir_base = Path(cli_parser.config.dsperse_run_dir) / f"run_{run_uid}"
    run_dir_base.mkdir(parents=True, exist_ok=True)

    try:
        input_json_path = run_dir_base / "input.json"
        with open(input_json_path, "w") as f:
            json.dump(frame_input, f)

        runner = Runner(run_dir=run_dir_base)
        results = runner.run(input_json_path=input_json_path, slice_path=slice_path)
        run_dir = runner.last_run_dir

        slice_results: dict[str, ExecutionInfo] = results.get("slice_results", {})

        if not all(info.success for info in slice_results.values()):
            failed = [k for k, v in slice_results.items() if not v.success]
            return FrameWitnessResult(
                frame_idx=frame_idx,
                run_uid=run_uid,
                slices=[],
                success=False,
                error=f"Slices failed: {failed}",
            )

        slices = DSperseManager._extract_dslice_data(
            slice_results, run_dir, circuit_id, frame_idx
        )

        return FrameWitnessResult(
            frame_idx=frame_idx,
            run_uid=run_uid,
            slices=slices,
            success=True,
        )

    except Exception as e:
        return FrameWitnessResult(
            frame_idx=frame_idx,
            run_uid=run_uid,
            slices=[],
            success=False,
            error=str(e),
        )
