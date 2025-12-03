import random
import uuid

from bittensor import logging

from deployment_layer.circuit_store import circuit_store
from execution_layer.circuit import CircuitType, Circuit
from _validator.api import ValidatorAPI


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
        run_uid = str(uuid.uuid4())
        logging.info(
            f"Generating DSlice requests for circuit {circuit.metadata.name}... Run UID: {run_uid}"
        )

        # TODO: ...
        self.run_dsperse(circuit, run_uid)
        dslice_requests = []
        # Logic to create DSlice requests goes here
        return dslice_requests

    def run_dsperse(self, circuit: Circuit, run_uid: str) -> None:
        return []

        # # Create temporary folder for run metadata
        # run_metadata_path = Path(tempfile.mkdtemp(prefix=f"dsperse_run_{run_uid}_"))
        # save_metadata_path = run_metadata_path / "metadata.json"
        # logging.info(f"Running DSperse model. Run metadata path: {run_metadata_path}")

        # # Generate benchmarking input JSON
        # input_json_path = run_metadata_path / "input.json"
        # with open(input_json_path, "w") as f:
        #     json.dump(circuit.input_handler(RequestType.BENCHMARK).generate(), f)

        # # init runner and run the sliced model
        # runner = Runner(
        #     run_metadata_path=run_metadata_path, save_metadata_path=save_metadata_path
        # )
        # results = runner.run(input_json_path=input_json_path, slice_path=slices_path)
