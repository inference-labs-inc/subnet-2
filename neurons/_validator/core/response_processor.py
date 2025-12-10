from __future__ import annotations

import time
import traceback

import bittensor as bt
from execution_layer.dsperse_manager import DSperseManager
from execution_layer.generic_input import GenericInput
from execution_layer.verified_model_session import VerifiedModelSession
from substrateinterface import Keypair

from _validator.core.request import Request
from _validator.models.completed_proof_of_weights import CompletedProofOfWeightsItem
from _validator.models.miner_response import MinerResponse
from _validator.models.request_type import RequestType
from _validator.scoring.score_manager import ScoreManager


class ResponseProcessor:
    def __init__(
        self,
        metagraph,
        score_manager: ScoreManager,
        dsperse_manager: DSperseManager,
        user_uid,
        hotkey: Keypair,
    ):
        self.metagraph = metagraph
        self.score_manager = score_manager
        self.dsperse_manager = dsperse_manager
        self.user_uid = user_uid
        self.hotkey = hotkey
        self.proof_batches_queue = []
        self.completed_proof_of_weights_queue: list[CompletedProofOfWeightsItem] = []

    def process_single_response(self, response: Request | None) -> MinerResponse | None:
        if response is None:
            return None
        miner_response = MinerResponse.from_raw_response(response)
        if not miner_response.proof_content:
            bt.logging.debug(
                f"Miner at UID: {miner_response.uid} failed to provide a valid proof for "
                f"{str(miner_response.circuit)}."
                f"Response from miner: {miner_response.raw}"
            )
        else:
            bt.logging.debug(
                f"Attempting to verify proof for UID: {miner_response.uid} "
                f"using {str(miner_response.circuit)}."
            )
            try:
                start_time = time.time()
                verification_result = self.verify_proof_string(
                    miner_response, response.inputs
                )
                miner_response.verification_time = time.time() - start_time
                miner_response.set_verification_result(verification_result)
                if not verification_result:
                    bt.logging.debug(
                        f"Miner at UID: {miner_response.uid} provided a proof"
                        f" for {str(miner_response.circuit)}"
                        ", but verification failed."
                    )
            except Exception as e:
                bt.logging.debug(
                    f"Unable to verify proof for UID: {miner_response.uid}. Error: {e}"
                )
                traceback.print_exc()

            if miner_response.verification_result:
                bt.logging.debug(
                    f"Miner at UID: {miner_response.uid} provided a valid proof "
                    f"for {str(miner_response.circuit)} "
                    f"in {miner_response.response_time} seconds."
                )
        return miner_response

    def verify_proof_string(
        self, response: MinerResponse, validator_inputs: GenericInput
    ) -> bool:
        if not response.proof_content:
            bt.logging.error(f"Proof not found for UID: {response.uid}")
            return False
        try:
            if response.request_type == RequestType.DSLICE:
                res = self.dsperse_manager.verify_slice_proof(
                    run_uid=response.dsperse_run_uid,
                    slice_num=response.dsperse_slice_num,
                    proof=response.proof_content,
                )
            else:
                if not response.public_json:
                    raise ValueError(
                        f"Public signals not found in for UID: {response.uid}"
                    )
                inference_session = VerifiedModelSession(
                    GenericInput(RequestType.RWR, response.public_json),
                    response.circuit,
                )
                res: bool = inference_session.verify_proof(
                    validator_inputs, response.proof_content
                )
                inference_session.end()
            return res
        except Exception as e:
            raise e
