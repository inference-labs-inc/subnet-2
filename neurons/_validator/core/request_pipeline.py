from __future__ import annotations

import copy
import random
import traceback

import bittensor as bt
from bittensor.core.chain_data import AxonInfo

from _validator.api import ValidatorAPI
from _validator.config import ValidatorConfig
from _validator.core.request import Request
from _validator.models.base_rpc_request import QueuedRequestDataModel
from _validator.models.request_type import RequestType
from _validator.pow.proof_of_weights_handler import ProofOfWeightsHandler
from _validator.scoring.score_manager import ScoreManager
from _validator.utils.hash_guard import HashGuard
from constants import (
    BATCHED_PROOF_OF_WEIGHTS_MODEL_ID,
    SINGLE_PROOF_OF_WEIGHTS_MODEL_ID,
)
from deployment_layer.circuit_store import circuit_store
from execution_layer.circuit import Circuit, CircuitType
from execution_layer.generic_input import GenericInput
from protocol import (
    ProofOfWeightsDataModel,
    QueryZkProof,
    DSliceProofGenerationDataModel,
)
from utils.wandb_logger import safe_log


class RequestPipeline:
    def __init__(
        self, config: ValidatorConfig, score_manager: ScoreManager, api: ValidatorAPI
    ):
        self.config = config
        self.score_manager = score_manager
        self.api = api
        self.hash_guard = HashGuard()

    def _check_and_create_request(
        self,
        uid: int,
        request_data: (
            ProofOfWeightsDataModel | QueryZkProof | DSliceProofGenerationDataModel
        ),
        circuit: Circuit,
        request_type: RequestType,
        external_request_hash: str | None = None,
        save: bool = False,
    ) -> Request | None:
        """Check hash and create request if valid."""
        try:
            if isinstance(request_data, ProofOfWeightsDataModel) or isinstance(
                request_data, DSliceProofGenerationDataModel
            ):
                input_data = request_data.inputs
            else:
                input_data = request_data.query_input
            # Check hash to prevent duplicate requests
            guard_hash = self.hash_guard.check_hash(input_data)
        except ValueError as e:
            bt.logging.error(f"Hash already exists: {e}")
            safe_log({"hash_guard_error": 1})
            if request_type == RequestType.RWR:
                self.api.set_request_result(
                    external_request_hash,
                    {"success": False, "error": "Hash already exists"},
                )
            return None

        axon: AxonInfo = self.config.metagraph.axons[uid]

        request = Request(
            uid=uid,
            ip=axon.ip,
            port=axon.port,
            hotkey=axon.hotkey,
            coldkey=axon.coldkey,
            data=request_data.model_dump(),
            url_path=request_data.name,
            circuit=circuit,
            request_type=request_type,
            # 'inputs' are used for verification later on validator side:
            #   I suppose `RWR` passed here to prevent new data generation
            inputs=GenericInput(RequestType.RWR, input_data),
            external_request_hash=external_request_hash,
            guard_hash=guard_hash,
            save=save,
        )

        if isinstance(request_data, DSliceProofGenerationDataModel):
            # Add dsperse specific fields
            request.dsperse_slice_num = request_data.slice_num
            request.dsperse_run_uid = request_data.run_uid

        return request

    def _prepare_queued_request(self, uid: int) -> Request:
        external_request = self.api.stacked_requests_queue.pop()
        request = None

        try:
            request_data, save = self.get_request_data(
                external_request.request_type,
                external_request.circuit,
                external_request,
            )
            request = self._check_and_create_request(
                uid=uid,
                request_data=request_data,
                circuit=external_request.circuit,
                request_type=external_request.request_type,
                external_request_hash=external_request.hash,
                save=save,
            )
            if request:
                request.queued_request = external_request
        except Exception as e:
            bt.logging.error(f"Error preparing request for UID {uid}: {e}")
            traceback.print_exc()
            if external_request.request_type == RequestType.RWR:
                self.api.set_request_result(
                    external_request.hash,
                    {"success": False, "error": "Error preparing request"},
                )
        return request

    def _prepare_benchmark_request(self, uid: int) -> Request:
        circuit = self.select_circuit_for_benchmark()
        if circuit is None:
            bt.logging.error("No circuit selected")
            return None

        request_data, save = self.get_request_data(RequestType.BENCHMARK, circuit)
        return self._check_and_create_request(
            uid=uid,
            request_data=request_data,
            circuit=circuit,
            request_type=RequestType.BENCHMARK,
            save=save,
        )

    def select_circuit_for_benchmark(self) -> Circuit:
        """
        Select a circuit for benchmarking using weighted random selection.
        """
        circuits = list(circuit_store.circuits.values())

        return random.choices(
            circuits,
            weights=[
                (circuit.metadata.benchmark_choice_weight or 0) for circuit in circuits
            ],
            k=1,
        )[0]

    def get_request_data(
        self,
        request_type: RequestType,
        circuit: Circuit,
        request: any | None = None,
    ) -> tuple[ProofOfWeightsDataModel | QueryZkProof, bool]:
        inputs = (
            circuit.input_handler(request_type)
            if request_type == RequestType.BENCHMARK
            else circuit.input_handler(
                request_type,
                copy.deepcopy(request.inputs),
            )
        )
        inputs = inputs.to_json() if hasattr(inputs, "to_json") else inputs

        if request_type == RequestType.RWR:
            if circuit.metadata.type == CircuitType.PROOF_OF_WEIGHTS:
                return (
                    ProofOfWeightsDataModel(
                        subnet_uid=circuit.metadata.netuid,
                        verification_key_hash=circuit.id,
                        proof_system=circuit.proof_system,
                        inputs=inputs,
                        proof="",
                        public_signals="",
                    ),
                    True,
                )
            return (
                QueryZkProof(query_input=inputs, model_id=circuit.id, query_output=""),
                True,
            )

        if circuit.id in [
            SINGLE_PROOF_OF_WEIGHTS_MODEL_ID,
            BATCHED_PROOF_OF_WEIGHTS_MODEL_ID,
        ]:
            request_data, save = ProofOfWeightsHandler.prepare_pow_request(
                circuit, self.score_manager
            )
            if request_data:
                return request_data, save
        if circuit.metadata.type == CircuitType.PROOF_OF_COMPUTATION:
            return (
                QueryZkProof(query_input=inputs, model_id=circuit.id, query_output=""),
                False,
            )
        elif circuit.metadata.type == CircuitType.DSPERSE_PROOF_GENERATION:
            return (
                DSliceProofGenerationDataModel(
                    circuit=circuit.id,
                    inputs=request.inputs,
                    outputs=request.outputs,
                    slice_num=request.slice_num,
                    run_uid=request.run_uid,
                ),
                False,
            )

        return (
            ProofOfWeightsDataModel(
                subnet_uid=circuit.metadata.netuid,
                verification_key_hash=circuit.id,
                proof_system=circuit.proof_system,
                inputs=inputs,
                proof="",
                public_signals="",
            ),
            False,
        )
