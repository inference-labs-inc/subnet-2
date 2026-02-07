from __future__ import annotations

import time

import bittensor as bt
from execution_layer.dsperse_manager import DSperseManager
from execution_layer.generic_input import GenericInput
from execution_layer.verified_model_session import VerifiedModelSession

from _validator.core.exceptions import EmptyProofException, IncorrectProofException
from _validator.core.request import Request
from _validator.models.miner_response import MinerResponse
from _validator.models.request_type import RequestType


class ResponseProcessor:
    def __init__(self, dsperse_manager: DSperseManager):
        self.dsperse_manager = dsperse_manager

    def verify_single_response(
        self, request: Request, miner_response: MinerResponse
    ) -> MinerResponse | None:
        """
        Verify a single response from a miner

        Raises:
            EmptyProofException: If miner fails to provide a proof.
            IncorrectProofException: If proof verification fails.
        """
        circuit_str = str(miner_response.circuit)

        if not miner_response.proof_content:
            bt.logging.error(
                f"Miner at UID: {miner_response.uid} failed to provide a valid proof for "
                f"{circuit_str}. Response from miner: {miner_response.raw}"
            )
            raise EmptyProofException(
                uid=miner_response.uid,
                circuit=circuit_str,
                raw_response=miner_response.raw,
            )

        bt.logging.debug(
            f"Attempting to verify proof for UID: {miner_response.uid} "
            f"using {circuit_str}."
        )

        start_time = time.time()
        verification_result = self._verify_response_proof(request, miner_response)
        miner_response.verification_time = time.time() - start_time
        miner_response.verification_result = verification_result

        if not verification_result:
            bt.logging.debug(
                f"Miner at UID: {miner_response.uid} provided a proof"
                f" for {circuit_str}, but verification failed."
            )
            raise IncorrectProofException(
                uid=miner_response.uid,
                circuit=circuit_str,
            )

        bt.logging.debug(
            f"Miner at UID: {miner_response.uid} provided a valid proof "
            f"for {circuit_str} in {miner_response.response_time} seconds."
        )
        return miner_response

    def _verify_response_proof(self, request: Request, response: MinerResponse) -> bool:
        """
        Verify the proof contained in the miner's response.
        """
        if not response.proof_content:
            bt.logging.error(f"Proof not found for UID: {response.uid}")
            return False

        if response.request_type == RequestType.DSLICE:
            if response.is_incremental and response.witness:
                inputs = request.inputs
                if inputs is not None and hasattr(inputs, "to_json"):
                    inputs = inputs.to_json()
                elif inputs is None:
                    inputs = request.data.get("inputs", {})
                res, extracted_outputs = (
                    self.dsperse_manager.verify_incremental_slice_with_witness(
                        circuit_id=response.circuit.id,
                        slice_num=str(response.dsperse_slice_num),
                        original_inputs=inputs,
                        witness_hex=response.witness,
                        proof_hex=response.proof_content,
                        proof_system=request.data.get("proof_system"),
                    )
                )
                if res and extracted_outputs is not None:
                    response.computed_outputs = extracted_outputs
            else:
                res: bool = self.dsperse_manager.verify_slice_proof(
                    run_uid=response.dsperse_run_uid,
                    slice_num=response.dsperse_slice_num,
                    proof=response.proof_content,
                    proof_system=request.data.get("proof_system"),
                )
        else:
            if not response.public_json:
                raise ValueError(f"Public signals not found for UID: {response.uid}")
            inference_session = VerifiedModelSession(
                GenericInput(RequestType.RWR, response.public_json),
                response.circuit,
            )
            res: bool = inference_session.verify_proof(
                response.inputs, response.proof_content
            )
            inference_session.end()
        return res
