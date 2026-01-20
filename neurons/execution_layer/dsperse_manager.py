import json
import random
import tempfile
import shutil
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Iterable

from bittensor import logging
from deployment_layer.circuit_store import circuit_store
from dsperse.src.backends.jstprove import JSTprove
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
    proof_system: ProofSystem
    witness_file: Path | None = None
    proof_file: Path | None = None
    success: bool | None = None


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
                        slice_num=slice_data.slice_num,
                        run_uid=run_uid,
                        proof_system=slice_data.proof_system,
                    )

    def run_dsperse(
        self,
        circuit: Circuit,
        run_uid: str,
    ) -> list[DSliceData]:
        # Create temporary folder for run metadata
        run_metadata_path = Path(cli_parser.config.dsperse_run_dir) / f"run_{run_uid}"
        run_metadata_path.mkdir(parents=True, exist_ok=True)
        save_metadata_path = run_metadata_path / "metadata.json"
        logging.debug(f"Running DSperse model. Run metadata path: {run_metadata_path}")

        # Generate benchmarking input JSON
        input_json_path = run_metadata_path / "input.json"
        with open(input_json_path, "w") as f:
            json.dump(circuit.input_handler(RequestType.BENCHMARK).generate(), f)

        # init runner and run the sliced model
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

        return [
            DSliceData(
                slice_num=slice_num.split("_")[-1],
                input_file=Path(r["input_file"]),
                output_file=Path(r["output_file"]),
                witness_file=Path(r["witness_file"]) if r.get("witness_file") else None,
                circuit_id=circuit.id,
                proof_system=self._get_proof_system_for_run(r),
            )
            for slice_num, r in slice_results.items()
            if not r["method"].startswith("onnx")
        ]

    def prove_slice(
        self,
        circuit_id: str,
        slice_num: str,
        inputs: dict,
        outputs: dict,
        proof_system: ProofSystem,
    ) -> dict:
        """
        Generate proof for a given slice.
        """
        circuit = self._get_circuit_by_id(circuit_id)
        model_dir = Path(circuit.paths.external_base_path) / f"slice_{slice_num}"
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

            with open(output_file, "w") as f:
                json.dump(outputs, f)

            if proof_system == ProofSystem.JSTPROVE:
                jstprover = JSTprove()
                jst_model_path = (
                    model_dir
                    / "payload"
                    / "jstprove"
                    / f"slice_{slice_num}_circuit.txt"
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
                run_path=tmp_path,
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

    def verify_slice_proof(
        self, run_uid: str, slice_num: str, proof: dict | str, proof_system: ProofSystem
    ) -> bool:
        """
        Verify proof for a given slice.
        """
        # do we have run data for this run UID?
        if run_uid not in self.runs:
            raise ValueError(f"Run UID {run_uid} not found.")

        # get slice run data from stored run data
        slice_data: DSliceData = next(
            (s for s in self.runs[run_uid] if s.slice_num == slice_num), None
        )
        if slice_data is None:
            raise ValueError(f"Slice data for slice number {slice_num} not found.")
        if slice_data.proof_system != proof_system:
            raise ValueError(
                f"Proof system mismatch for slice {slice_num} in run {run_uid}. Expected {slice_data.proof_system}, got {proof_system}."
            )

        circuit = self._get_circuit_by_id(slice_data.circuit_id)

        proof_file_path = slice_data.input_file.parent / "proof.json"
        if proof_system == ProofSystem.JSTPROVE:
            # for JSTPROVE, proof is a hex string of bytes
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
            # for other proof systems (now only EZKL), proof is a JSON object
            with open(proof_file_path, "w") as proof_file:
                json.dump(proof, proof_file)

        slice_data.proof_file = proof_file_path

        # time to verify!
        verifier = Verifier()
        result = verifier.verify(
            run_path=slice_data.input_file.parent,
            model_path=Path(circuit.paths.external_base_path) / f"slice_{slice_num}",
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
        raise ValueError(
            f"Unknown proof method '{method}' - cannot determine proof system"
        )

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
