from __future__ import annotations

import time

import bittensor as bt
from execution_layer.dsperse_manager import DSperseManager
from execution_layer.generic_input import GenericInput
from execution_layer.verified_model_session import VerifiedModelSession

from _validator.core.exceptions import EmptyProofException, IncorrectProofException
from _validator.models.miner_response import MinerResponse
from _validator.models.request_type import RequestType


class ResponseProcessor:
    def __init__(self, dsperse_manager: DSperseManager):
        self.dsperse_manager = dsperse_manager

    def verify_single_response(
        self, miner_response: MinerResponse
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
        verification_result = self._verify_response_proof(
            miner_response, miner_response.inputs
        )
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

    def _verify_response_proof(
        self, response: MinerResponse, validator_inputs: GenericInput
    ) -> bool:
        """
        Verify the proof contained in the miner's response.
        """
        if not response.proof_content:
            bt.logging.error(f"Proof not found for UID: {response.uid}")
            return False

        if response.request_type == RequestType.DSLICE:
            res = self.dsperse_manager.verify_slice_proof(
                run_uid=response.dsperse_run_uid,
                slice_num=response.dsperse_slice_num,
                proof=response.proof_content,
                proof_system=response.inputs.data.get("proof_system"),  # TODO: test it
            )
            # Check if the entire DSperse run is complete and clean up if so:
            self.dsperse_manager.check_run_completion(
                run_uid=response.dsperse_run_uid, remove_completed=True
            )
        else:
            if not response.public_json:
                raise ValueError(f"Public signals not found for UID: {response.uid}")
            inference_session = VerifiedModelSession(
                # hardcoded request type as RWR because we don't want to regenerate inputs
                GenericInput(RequestType.RWR, response.public_json),
                response.circuit,
            )
            res: bool = inference_session.verify_proof(
                validator_inputs, response.proof_content
            )
            inference_session.end()
        return res
