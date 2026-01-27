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
from dsperse.src.compile.compiler import Compiler
from dsperse.src.prover import Prover
from dsperse.src.run.runner import Runner
from dsperse.src.verifier import Verifier
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
    prove_system: ProofSystem
    witness_file: Path | None = None
    proof_file: Path | None = None
    success: bool | None = None
    frame_idx: int | None = None
    retry_count: int = 0


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
        self.runs = {}  # run_uid -> run data (slices etc.), used by validator only

    def _get_circuit_by_id(self, circuit_id: str) -> Circuit | None:
        circuit = next((c for c in self.circuits if c.id == circuit_id), None)
        if circuit is None:
            raise ValueError(f"Circuit with ID {circuit_id} not found.")
        return circuit

    def generate_dslice_requests(self) -> Iterable[DSliceQueuedProofRequest]:
        """
        Generate DSlice requests for DSperse models.
        Each DSlice request corresponds to one slice of a DSperse model.
        """
        if not self.circuits:
            # No DSperse circuits available, skip request generation
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
                        witness=(
                            slice_data.witness_file.read_bytes()
                            if slice_data.witness_file
                            else None
                        ),
                        slice_num=slice_data.slice_num,
                        run_uid=run_uid,
                        proof_system=slice_data.prove_system,
                    )

    def run_dsperse(
        self,
        circuit: Circuit,
        run_uid: str,
    ) -> list[DSliceData]:
        run_metadata_path = Path(cli_parser.config.dsperse_run_dir) / f"run_{run_uid}"
        run_metadata_path.mkdir(parents=True, exist_ok=True)
        save_metadata_path = run_metadata_path / "metadata.json"
        logging.debug(f"Running DSperse model. Run metadata path: {run_metadata_path}")

        input_json_path = run_metadata_path / "input.json"
        with open(input_json_path, "w") as f:
            json.dump(circuit.input_handler(RequestType.BENCHMARK).generate(), f)

        runner = Runner(save_metadata_path=save_metadata_path)
        results = runner.run(
            input_json_path=input_json_path, slice_path=circuit.paths.external_base_path
        )
        logging.debug(
            f"DSperse run completed. Results data saved at {save_metadata_path}"
        )
        slice_results = results["slice_results"]

        if not all(slice_result["success"] for slice_result in slice_results.values()):
            logging.error(
                "DSperse run failed for some slices. Aborting request generation..."
            )
            return []

        dslice_data_list = []
        for slice_num, r in slice_results.items():
            method = r.get("method", "")
            if method.startswith("onnx"):
                continue

            base_slice_num = slice_num.split("_")[-1]

            if method == "tiled_parallel":
                tile_exec_infos = r.get("tile_exec_infos", [])
                num_tiles = r.get("num_tiles", len(tile_exec_infos))
                logging.info(
                    f"Expanding tiled slice {slice_num} into {num_tiles} tile requests"
                )

                for tile_idx, tile_info in enumerate(tile_exec_infos):
                    tile_slice_num = f"{base_slice_num}_tile_{tile_idx}"
                    tile_input = tile_info.get("input_file")
                    tile_output = tile_info.get("output_file")
                    tile_witness = tile_info.get("witness_file")

                    if not tile_input or not tile_output:
                        logging.warning(
                            f"Skipping tile {tile_idx} of slice {slice_num}: missing input/output"
                        )
                        continue

                    dslice_data_list.append(
                        DSliceData(
                            slice_num=tile_slice_num,
                            input_file=Path(tile_input),
                            output_file=Path(tile_output),
                            witness_file=Path(tile_witness) if tile_witness else None,
                            circuit_id=circuit.id,
                            prove_system=self._get_proof_system_for_tile(tile_info),
                        )
                    )
            else:
                dslice_data_list.append(
                    DSliceData(
                        slice_num=base_slice_num,
                        input_file=Path(r["input_file"]),
                        output_file=Path(r["output_file"]),
                        witness_file=Path(r["witness_file"]) if r.get("witness_file") else None,
                        circuit_id=circuit.id,
                        prove_system=self._get_proof_system_for_run(r),
                    )
                )

        logging.info(f"Generated {len(dslice_data_list)} DSlice requests (tiles expanded)")
        return dslice_data_list

    def _get_proof_system_for_tile(self, tile_info: dict) -> ProofSystem:
        method = tile_info.get("method", "")
        if method.startswith("jstprove"):
            return ProofSystem.JSTPROVE
        elif method.startswith("ezkl"):
            return ProofSystem.EZKL
        raise ValueError(f"Unknown tile proof method '{method}'")

    def prove_slice(
        self,
        circuit_id: str,
        slice_num: str,
        inputs: dict,
        outputs: dict,
        witness: bytes | None,
        proof_system: ProofSystem,
    ) -> dict | None:
        """
        Generate proof for a given slice or tile.
        slice_num can be "0" (regular slice) or "0_tile_5" (tile 5 of slice 0).
        """
        circuit = self._get_circuit_by_id(circuit_id)

        base_slice_num, tile_idx = self._parse_slice_num(slice_num)
        model_dir = Path(circuit.paths.external_base_path) / f"slice_{base_slice_num}"

        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp_path = Path(tmp_dir)
            input_file = tmp_path / "input.json"

            with open(input_file, "w") as f:
                json.dump(inputs, f)

            runner = Runner(
                run_dir=tmp_path,
                parallel_tiles=16
            )
            run_result = runner.run(
                input_json_path=input_file,
                slice_path=model_dir
            )
            run_dir = runner.last_run_dir
            logging.info(f"Runner completed for slice_{slice_num}, run_dir: {run_dir}")

            if runner.run_metadata:
                metadata_path = run_dir / "metadata.json"
                with open(metadata_path, "w") as f:
                    json.dump(runner.run_metadata.to_dict(), f)

            prover = Prover()
            result = prover.prove(
                run_path=run_dir,
                model_dir=model_dir,
                output_path=tmp_path,
                backend=proof_system.value.lower(),
            )
            logging.debug(f"Got proof generation result. Result: {result}")

            _, proof_execution = self._parse_dsperse_result(result, "proof")

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

            return {
                "circuit_id": circuit_id,
                "slice_num": slice_num,
                "success": success,
                "proof_generation_time": proof_generation_time,
                "proof": proof_data,
            }

    def _parse_slice_num(self, slice_num: str) -> tuple[str, int | None]:
        """
        Parse slice_num to extract base slice and optional tile index.
        "0" -> ("0", None)
        "0_tile_5" -> ("0", 5)
        """
        if "_tile_" in slice_num:
            parts = slice_num.split("_tile_")
            return parts[0], int(parts[1])
        return slice_num, None

    def verify_slice_proof(
        self, run_uid: str, slice_num: str, proof: dict | str, proof_system: ProofSystem
    ) -> bool:
        """
        Verify proof for a given slice or tile.
        slice_num can be "0" (regular slice) or "0_tile_5" (tile 5 of slice 0).
        """
        if run_uid not in self.runs:
            raise ValueError(f"Run UID {run_uid} not found.")

        slice_data: DSliceData = next(
            (s for s in self.runs[run_uid] if s.slice_num == slice_num), None
        )
        if slice_data is None:
            raise ValueError(f"Slice data for slice number {slice_num} not found.")
        if slice_data.prove_system != proof_system:
            raise ValueError(
                f"Proof system mismatch for slice {slice_num} in run {run_uid}. Expected {slice_data.prove_system}, got {proof_system}."
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
            model_path=Path(circuit.paths.external_base_path) / f"slice_{base_slice_num}",
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
        """
        Check if all slices in a run have been successfully verified.
        """
        if run_uid not in self.runs:
            raise ValueError(f"Run UID {run_uid} not found.")

        slices: list[DSliceData] = self.runs[run_uid]
        all_verified = all(slice_data.success for slice_data in slices)
        if all_verified and remove_completed:
            self.cleanup_run(run_uid)
        return all_verified

    def cleanup_run(self, run_uid: str):
        """
        Cleanup run data and delete run folder for a given run UID.
        """
        if run_uid not in self.runs:
            raise ValueError(f"Cannot cleanup run data. Run UID {run_uid} not found.")
        logging.info(f"Cleaning up run data for run UID {run_uid}...")
        run_path = self.runs[run_uid][0].input_file.parent.parent
        if run_path.exists() and run_path.is_dir():
            shutil.rmtree(run_path)
        del self.runs[run_uid]

    def total_cleanup(self):
        """
        Cleanup all run data and delete all run folders.
        Used during validator shutdown to free up disk space.
        """
        logging.info("Performing total cleanup of all DSperse run data...")
        for run_uid in list(self.runs.keys()):
            self.cleanup_run(run_uid)

    def _get_proof_system_for_run(self, result: dict) -> ProofSystem:
        method = result.get("method", "")
        if method.startswith("jstprove"):
            return ProofSystem.JSTPROVE
        elif method.startswith("ezkl"):
            return ProofSystem.EZKL
        raise ValueError(f"Unknown proof method '{method}' - cannot determine proof system")

    def _parse_dsperse_result(
        self, result: dict, execution_type: str
    ) -> tuple[str | None, dict]:
        execution_results = result.get("execution_chain", {}).get(
            "execution_results", []
        )
        execution_result = execution_results[0] if execution_results else {}
        if not execution_result:
            logging.error(f"No execution results found in proof generation result.")

        slice_id = execution_result.get("slice_id", None)
        execution = execution_result.get(f"{execution_type}_execution", {})

        if execution_type == "proof" and not execution.get("proof_file"):
            tile_proofs = execution.get("tile_proofs_info") or []
            if tile_proofs and len(tile_proofs) > 0:
                tile_0 = tile_proofs[0]
                if isinstance(tile_0, dict) and tile_0.get("proof_path"):
                    execution["proof_file"] = tile_0["proof_path"]
                    if tile_0.get("success") is not None:
                        execution["success"] = tile_0["success"]

        return slice_id, execution

    @classmethod
    def extract_dslices(cls, model_path: Path | str) -> None:
        """
        Extract DSperse slice files in a folder if there are any.
        """
        model_path = Path(model_path)
        # dslice_files = glob.glob(os.path.join(model_path, "slice_*.dslice"))
        dslice_files = list(model_path.glob("slice_*.dslice"))
        if not dslice_files:
            return
        logging.debug(SYNC_LOG_PREFIX + f"Extracting DSlices for model {model_path}...")
        for dslice_file in dslice_files:
            # extracted_path = os.path.splitext(dslice_file)[0]
            extracted_path = dslice_file.with_suffix("")  # remove .dslice suffix
            if extracted_path.exists():
                # Extracted folder already exists, but the .dslice file is not deleted
                # that means we probably interrupted extraction previously. Let's extract again
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
            # `cleanup=True` doesn't work for some reason, so we manually delete the .dslice file
            dslice_file.unlink(missing_ok=True)

    def generate_batch_witnesses(
        self,
        circuit: Circuit,
        frame_inputs: list[dict],
        max_workers: int = 8,
    ) -> BatchRun:
        """
        Generate witnesses for multiple frames in parallel using ProcessPoolExecutor.
        Each frame is processed independently, allowing true parallelism.
        """
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
        """Convert successful frame results into DSlice proof requests."""
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

                    witness = None
                    if slice_data.witness_file and slice_data.witness_file.exists():
                        witness = slice_data.witness_file.read_bytes()

                    request = DSliceQueuedProofRequest(
                        circuit=circuit,
                        inputs=inputs,
                        outputs=outputs,
                        witness=witness,
                        slice_num=slice_data.slice_num,
                        run_uid=result.run_uid,
                        proof_system=slice_data.prove_system,
                    )
                    batch_run.pending_slices.append(request)
                except Exception as e:
                    logging.error(
                        f"Failed to create request for frame {frame_idx} "
                        f"slice {slice_data.slice_num}: {e}"
                    )

        logging.info(
            f"Batch {batch_run.batch_id}: {len(batch_run.pending_slices)} "
            f"proof requests ready for distribution"
        )

    def retry_failed_slice(
        self, batch_run: BatchRun, failed_request: DSliceQueuedProofRequest, max_retries: int = 3
    ) -> bool:
        """
        Retry a failed slice proof request.
        Returns True if the request should be retried, False if max retries exceeded.
        """
        run_uid = failed_request.run_uid
        slice_num = failed_request.slice_num

        if run_uid not in self.runs:
            logging.error(f"Cannot retry: run {run_uid} not found")
            return False

        slice_data = next(
            (s for s in self.runs[run_uid] if s.slice_num == slice_num), None
        )
        if slice_data is None:
            logging.error(f"Cannot retry: slice {slice_num} not found in run {run_uid}")
            return False

        slice_data.retry_count += 1
        if slice_data.retry_count > max_retries:
            logging.warning(
                f"Slice {slice_num} in run {run_uid} exceeded max retries ({max_retries})"
            )
            batch_run.failed_slices.append(failed_request)
            return False

        logging.info(
            f"Retrying slice {slice_num} in run {run_uid} "
            f"(attempt {slice_data.retry_count}/{max_retries})"
        )
        batch_run.pending_slices.append(failed_request)
        return True

    def mark_slice_complete(
        self, batch_run: BatchRun, run_uid: str, slice_num: str
    ) -> None:
        """Mark a slice as successfully proven."""
        slice_key = f"{run_uid}:{slice_num}"
        if slice_key not in batch_run.completed_slices:
            batch_run.completed_slices.append(slice_key)

    def get_batch_progress(self, batch_run: BatchRun) -> dict:
        """Get progress statistics for a batch run."""
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
    """
    Process a single frame's witness generation.
    This function runs in a separate process via ProcessPoolExecutor.
    """
    run_uid = f"{batch_id}_{frame_idx}"
    run_dir = Path(cli_parser.config.dsperse_run_dir) / f"run_{run_uid}"
    run_dir.mkdir(parents=True, exist_ok=True)

    try:
        input_json_path = run_dir / "input.json"
        with open(input_json_path, "w") as f:
            json.dump(frame_input, f)

        save_metadata_path = run_dir / "metadata.json"
        runner = Runner(save_metadata_path=save_metadata_path)
        results = runner.run(input_json_path=input_json_path, slice_path=slice_path)

        slice_results = results.get("slice_results", {})
        if not all(r.get("success", False) for r in slice_results.values()):
            failed = [k for k, v in slice_results.items() if not v.get("success")]
            return FrameWitnessResult(
                frame_idx=frame_idx,
                run_uid=run_uid,
                slices=[],
                success=False,
                error=f"Slices failed: {failed}",
            )

        slices = []
        for slice_num, r in slice_results.items():
            method = r.get("method", "")
            if method.startswith("onnx"):
                continue

            base_slice_num = slice_num.split("_")[-1]

            if method == "tiled_parallel":
                tile_exec_infos = r.get("tile_exec_infos", [])
                for tile_idx, tile_info in enumerate(tile_exec_infos):
                    tile_slice_num = f"{base_slice_num}_tile_{tile_idx}"
                    tile_input = tile_info.get("input_file")
                    tile_output = tile_info.get("output_file")
                    tile_witness = tile_info.get("witness_file")

                    if not tile_input or not tile_output:
                        continue

                    tile_method = tile_info.get("method", "")
                    if tile_method.startswith("jstprove"):
                        prove_system = ProofSystem.JSTPROVE
                    elif tile_method.startswith("ezkl"):
                        prove_system = ProofSystem.EZKL
                    else:
                        continue

                    slices.append(
                        DSliceData(
                            slice_num=tile_slice_num,
                            circuit_id=circuit_id,
                            input_file=Path(tile_input),
                            output_file=Path(tile_output),
                            witness_file=Path(tile_witness) if tile_witness else None,
                            prove_system=prove_system,
                            frame_idx=frame_idx,
                        )
                    )
            else:
                if method.startswith("jstprove"):
                    prove_system = ProofSystem.JSTPROVE
                elif method.startswith("ezkl"):
                    prove_system = ProofSystem.EZKL
                else:
                    continue

                slices.append(
                    DSliceData(
                        slice_num=base_slice_num,
                        circuit_id=circuit_id,
                        input_file=Path(r["input_file"]),
                        output_file=Path(r["output_file"]),
                        witness_file=Path(r["witness_file"]) if r.get("witness_file") else None,
                        prove_system=prove_system,
                        frame_idx=frame_idx,
                    )
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
