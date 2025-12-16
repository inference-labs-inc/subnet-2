import json
import random
import tempfile
import shutil
from dataclasses import dataclass
from datetime import datetime
from enum import Enum
from pathlib import Path
from typing import Iterable

import ezkl
from bittensor import logging
from deployment_layer.circuit_store import circuit_store
from dsperse.src.compile.compiler import Compiler
from dsperse.src.prover import Prover
from dsperse.src.run.runner import Runner
from dsperse.src.verifier import Verifier
from dsperse.src.slice.utils.converter import Converter
from execution_layer.circuit import Circuit, CircuitType

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
    proof_file: Path | None = None
    success: bool | None = None


class EZKLInputType(Enum):
    F16 = ezkl.PyInputType.F16
    F32 = ezkl.PyInputType.F32
    F64 = ezkl.PyInputType.F64
    Int = ezkl.PyInputType.Int
    Bool = ezkl.PyInputType.Bool
    TDim = ezkl.PyInputType.TDim


def ensure_proof_inputs(proof: dict, inputs: list[list], model_settings: dict) -> dict:
    """
    Ensures that the proof JSON contains the correct input instances.
    That should prevent miners from cheating by reusing proofs with different inputs.
    """
    scale_map = model_settings.get("model_input_scales", [])
    type_map = model_settings.get("input_types", [])
    instances = [
        ezkl.float_to_felt(x, scale_map[i], EZKLInputType[type_map[i]].value)
        for i, arr in enumerate(inputs)
        for x in arr
    ]
    proof["instances"] = [instances[:] + proof["instances"][0][len(instances) :]]

    proof["transcript_type"] = "EVM"

    return proof


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
        # do we have run data for this run UID?
        if run_uid not in self.runs:
            raise ValueError(f"Run UID {run_uid} not found.")

        # get slice run data from stored run data
        slice_data: DSliceData = next(
            (s for s in self.runs[run_uid] if s.slice_num == slice_num), None
        )
        if slice_data is None:
            raise ValueError(f"Slice data for slice number {slice_num} not found.")

        circuit = self._get_circuit_by_id(slice_data.circuit_id)
        # prepare inputs
        with open(slice_data.input_file, "r") as f:
            input_obj = circuit.input_handler(
                request_type=RequestType.DSLICE, data=json.load(f)
            )

        # ensure proof has correct inputs
        proof = ensure_proof_inputs(
            proof, input_obj.to_array(), self._get_slice_settings(circuit, slice_num)
        )

        proof_file_path = slice_data.input_file.parent / "proof.json"
        with open(proof_file_path, "w") as proof_file:
            json.dump(proof, proof_file)
        slice_data.proof_file = proof_file_path

        # time to verify!
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

    def total_cleanup(self):
        """
        Cleanup all run data and delete all run folders.
        Used during validator shutdown to free up disk space.
        """
        logging.info("Performing total cleanup of all DSperse run data...")
        for run_uid in list(self.runs.keys()):
            self.cleanup_run(run_uid)

    def _get_slice_settings(self, circuit: Circuit, slice_num: str) -> dict:
        """
        Retrieve settings for a specific slice from its metadata.
        """
        metadata = self.get_slice_metadata(
            Path(circuit.paths.external_base_path) / f"slice_{slice_num}"
        )

        settings_path = (
            metadata.get("slices", [{}])[0]
            .get("compilation", {})
            .get("ezkl", {})
            .get("files", {})
            .get("settings", None)
        )
        if not settings_path:
            raise ValueError(
                f"Settings file path not found in metadata for slice {slice_num} of circuit {circuit.id}."
            )
        settings_path = (
            Path(circuit.paths.external_base_path)
            / f"slice_{slice_num}"
            / settings_path
        )
        if not settings_path.exists() or not settings_path.is_file():
            raise ValueError(
                f"Settings file not found at {settings_path} for slice {slice_num} of circuit {circuit.id}."
            )
        with open(settings_path, "r") as f:
            settings = json.load(f)
        return settings

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
    def get_slice_metadata(cls, slice_path: Path | str) -> dict:
        """
        Retrieve metadata for a specific DSperse slice.
        """
        slice_path = Path(slice_path)
        metadata_path = slice_path / "metadata.json"
        if not metadata_path.exists():
            raise ValueError(f"Metadata file not found at {metadata_path}.")
        with open(metadata_path, "r") as f:
            metadata = json.load(f)
        if not isinstance(metadata, dict):
            raise ValueError(f"Invalid metadata format at {metadata_path}.")
        return metadata

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

    @classmethod
    def compile_dslices(cls, model_path: Path | str) -> None:
        """
        Compile DSperse slices in a folder if there are any.
        """
        model_path = Path(model_path)
        logging.debug(
            f"Checking compilation status for DSperse slices in {model_path.name}..."
        )
        compiler = Compiler()
        for slice_dir in model_path.glob("slice_*"):
            if not slice_dir.is_dir():
                continue

            metadata = cls.get_slice_metadata(slice_dir)
            is_compiled = (
                metadata.get("slices", [{}])[0]
                .get("compilation", {})
                .get("ezkl", {})
                .get("compiled", False)
            )
            if is_compiled:
                logging.debug(
                    f"DSlice {slice_dir.name} is already compiled. Skipping compilation."
                )
                continue

            logging.info(
                f"Compiling DSlice {slice_dir.name} in model {model_path.name}..."
            )
            compiler.compile(model_path=slice_dir)
