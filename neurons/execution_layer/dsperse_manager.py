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
from dsperse.src.prover import Prover
from dsperse.src.run.runner import Runner
from dsperse.src.verifier import Verifier
from execution_layer.circuit import Circuit, CircuitType

import cli_parser
from _validator.api import ValidatorAPI
from _validator.models.dslice_request import DSliceQueuedProofRequest
from _validator.models.request_type import RequestType


@dataclass
class DSliceData:
    slice_num: str
    circuit_id: str
    input_file: Path
    output_file: Path
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
            return []

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
                    )

    def run_dsperse(self, circuit: Circuit, run_uid: str) -> list[DSliceData]:
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
                circuit_id=circuit.id,
            )
            for slice_num, r in slice_results.items()
        ]

    def prove_slice(
        self, circuit_id: str, slice_num: str, inputs: dict, outputs: dict
    ) -> dict | None:
        """
        Generate proof for a given slice.
        """
        circuit = self._get_circuit_by_id(circuit_id)
        model_dir = Path(circuit.paths.external_base_path) / f"slice_{slice_num}"

        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp_path = Path(tmp_dir)

            input_file = tmp_path / "input.json"
            output_file = tmp_path / "output.json"

            with open(input_file, "w") as f:
                json.dump(inputs, f)

            with open(output_file, "w") as f:
                json.dump(outputs, f)

            prover = Prover()
            result = prover.prove(
                run_path=tmp_path,
                model_dir=model_dir,
                output_path=tmp_path,
            )
            logging.debug(f"Got proof generation result. Result: {result}")

            slice_id, proof_execution = self._parse_dsperse_result(result, "proof")

            success = proof_execution.get("success", False)
            proof_generation_time = proof_execution.get("proof_generation_time", None)
            proof_data = None
            if proof_execution.get("proof_file", None):
                with open(proof_execution["proof_file"], "r") as proof_file:
                    proof_data = json.load(proof_file)

            return {
                "circuit_id": circuit_id,
                "slice_num": slice_id,
                "success": success,
                "proof_generation_time": proof_generation_time,
                "proof": proof_data,
            }

    def verify_slice_proof(
        self,
        run_uid: str,
        slice_num: str,
        proof: dict,
    ) -> bool:
        """
        Verify proof for a given slice.
        """
        if run_uid not in self.runs:
            raise ValueError(f"Run UID {run_uid} not found.")

        slice_data: DSliceData = next(
            (s for s in self.runs[run_uid] if s.slice_num == slice_num), None
        )
        if slice_data is None:
            raise ValueError(f"Slice data for slice number {slice_num} not found.")

        circuit = self._get_circuit_by_id(slice_data.circuit_id)

        proof_file_path = slice_data.input_file.parent / "proof.json"
        with open(proof_file_path, "w") as proof_file:
            json.dump(proof, proof_file)
        slice_data.proof_file = proof_file_path

        verifier = Verifier()
        result = verifier.verify(
            run_path=slice_data.input_file.parent,
            model_path=Path(circuit.paths.external_base_path) / f"slice_{slice_num}",
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

    def _parse_dsperse_result(self, result: dict, execution_type: str) -> dict:
        execution_results = result.get("execution_chain", {}).get(
            "execution_results", []
        )
        execution_result = execution_results[0] if execution_results else None
        if not execution_result:
            logging.error(f"No execution results found in proof generation result.")
            return None

        slice_id = execution_result.get("slice_id", None)
        execution = execution_result.get(f"{execution_type}_execution", {})

        return slice_id, execution
