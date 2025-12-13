from dataclasses import dataclass

import bittensor as bt
import json

from constants import DEFAULT_PROOF_SIZE
from _validator.core.request import Request
from execution_layer.circuit import ProofSystem, Circuit
from _validator.models.request_type import RequestType


@dataclass
class MinerResponse:
    """
    Represents a response from a miner.
    """

    uid: int
    verification_result: bool
    # hash of the original request from external API user
    # we use it to report back results to `ValidatorAPI`` class. It sends the results to the user.
    external_request_hash: str
    response_time: float
    proof_size: int
    circuit: Circuit
    verification_time: float | None = None
    proof_content: dict | str | None = None
    public_json: list[str] | None = None
    inputs: dict | None = None
    request_type: RequestType | None = None
    dsperse_slice_num: int | None = None
    dsperse_run_uid: str | None = None
    raw: dict | None = None
    error: str | None = None
    save: bool = False

    @classmethod
    def from_raw_response(
        cls, request: Request, deserialized_response: dict
    ) -> "MinerResponse":
        """
        Creates a MinerResponse object from a raw response dictionary.
        """
        bt.logging.trace(f"Deserialized response: {deserialized_response}")

        proof = deserialized_response.get("proof", "{}")
        if isinstance(proof, str):
            if all(c in "0123456789ABCDEFabcdef" for c in proof):
                proof_content = proof
            else:
                proof_content = json.loads(proof)
        else:
            proof_content = proof

        if isinstance(proof_content, str):
            proof_size = len(proof_content)
        elif request.circuit is not None:
            if request.circuit.proof_system == ProofSystem.CIRCOM:
                proof_size = (
                    sum(
                        len(str(value))
                        for key in ("pi_a", "pi_b", "pi_c")
                        for element in proof_content.get(key, [])
                        for value in (
                            element if isinstance(element, list) else [element]
                        )
                    )
                    if proof_content
                    else DEFAULT_PROOF_SIZE
                )
            elif request.circuit.proof_system == ProofSystem.EZKL:
                proof_size = len(proof_content["proof"])
            else:
                proof_size = DEFAULT_PROOF_SIZE
        else:
            # capacity requests don't have circuit associated
            proof_size = 0

        public_signals = deserialized_response.get("public_signals", "[]")
        if public_signals and str(public_signals).strip():
            public_json = (
                json.loads(public_signals)
                if isinstance(public_signals, str)
                else public_signals
            )
        else:
            bt.logging.debug(f"Miner at {request.uid} did not return public signals.")
            public_json = None

        return cls(
            uid=request.uid,
            verification_result=False,
            response_time=request.response_time,
            proof_size=proof_size or DEFAULT_PROOF_SIZE,
            circuit=request.circuit,
            proof_content=proof_content,
            request_type=request.request_type,
            external_request_hash=request.external_request_hash,
            public_json=public_json,
            inputs=request.inputs,
            raw=deserialized_response,
            save=request.save,
            dsperse_slice_num=request.dsperse_slice_num,
            dsperse_run_uid=request.dsperse_run_uid,
        )

    def to_log_dict(self, metagraph: bt.metagraph) -> dict:  # type: ignore
        """
        Parse a MinerResponse object into a dictionary. Used for logging purposes.
        """
        return {
            "miner_key": metagraph.hotkeys[self.uid],
            "miner_uid": self.uid,
            "proof_model": (
                self.circuit.metadata.name
                if self.circuit is not None
                else str(self.circuit.id)
            ),
            "proof_system": (
                self.circuit.metadata.proof_system
                if self.circuit is not None
                else "Unknown"
            ),
            "proof_size": self.proof_size,
            "response_duration": self.response_time,
            "is_verified": self.verification_result,
            "external_request_hash": self.external_request_hash,
            "request_type": self.request_type.value,
            "error": self.error,
            "save": self.save,
        }

    def __iter__(self):
        return iter(self.__dict__.items())
