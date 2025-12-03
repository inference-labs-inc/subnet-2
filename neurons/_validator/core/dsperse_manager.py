import json
import random
from datetime import datetime
from pathlib import Path

from bittensor import logging
from dsperse.src.run.runner import Runner

import cli_parser
from _validator.api import ValidatorAPI
from _validator.models.dslice_request import DSliceQueuedProofRequest
from _validator.models.request_type import RequestType
from deployment_layer.circuit_store import circuit_store
from execution_layer.circuit import Circuit, CircuitType


class DSperseManager:
    def __init__(self, api: ValidatorAPI):
        self.api = api
        self.circuits: list[Circuit] = [
            circuit
            for circuit in circuit_store.circuits.values()
            if circuit.metadata.type == CircuitType.DSPERSE_PROOF_GENERATION
        ]
        self.runs = {}

    def generate_dslice_requests(self) -> list:
        """
        Generate DSlice requests for DSperse models.
        Each DSlice request corresponds to one slice of a DSperse model.
        """
        if self.api.stacked_requests_queue or not self.circuits:
            # there are already requests stacked, do not generate new DSlice requests
            return []

        circuit = random.choice(self.circuits)
        run_uid = datetime.now().strftime("%Y%m%d%H%M%S%f")
        logging.info(
            f"Generating DSlice requests for circuit {circuit.metadata.name}... Run UID: {run_uid}"
        )

        slices: list[dict] = self.run_dsperse(circuit, run_uid)
        dslice_requests = []

        for slice in slices:
            with open(slice["input_file"], "r") as input_file:
                with open(slice["output_file"], "r") as output_file:
                    self.api.stacked_requests_queue.insert(
                        0,
                        DSliceQueuedProofRequest(
                            circuit=circuit,
                            inputs=json.load(input_file),
                            outputs=json.load(output_file),
                            slice_num=slice["slice"],
                            run_uid=run_uid,
                        ),
                    )

        # Logic to create DSlice requests goes here
        return dslice_requests

    def run_dsperse(self, circuit: Circuit, run_uid: str) -> list[dict]:
        # Create temporary folder for run metadata
        run_metadata_path = Path(cli_parser.config.dsperse_run_dir) / f"run_{run_uid}"
        run_metadata_path.mkdir(parents=True, exist_ok=True)
        save_metadata_path = run_metadata_path / "metadata.json"
        logging.info(f"Running DSperse model. Run metadata path: {run_metadata_path}")

        # Generate benchmarking input JSON
        input_json_path = run_metadata_path / "input.json"
        with open(input_json_path, "w") as f:
            json.dump(circuit.input_handler(RequestType.BENCHMARK).generate(), f)

        # init runner and run the sliced model
        runner = Runner(save_metadata_path=save_metadata_path)
        results = runner.run(
            input_json_path=input_json_path, slice_path=circuit.paths.external_base_path
        )
        logging.info(
            f"DSperse run completed. Results data saved at {save_metadata_path}"
        )
        slice_results = results["slice_results"]

        if not all(slice_result["success"] for slice_result in slice_results.values()):
            logging.error(
                "DSperse run failed for some slices. Aborting request generation..."
            )
            return []

        return [
            {
                "slice": slice_num.split("_")[1],
                "input_file": r["input_file"],
                "output_file": r["output_file"],
            }
            for slice_num, r in slice_results.items()
        ]
