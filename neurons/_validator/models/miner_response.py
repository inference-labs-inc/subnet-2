from __future__ import annotations
from dataclasses import dataclass

import bittensor as bt
import json
import traceback

from constants import (
    DEFAULT_PROOF_SIZE,
    SINGLE_PROOF_OF_WEIGHTS_MODEL_ID,
    CIRCUIT_TIMEOUT_SECONDS,
)
from deployment_layer.circuit_store import circuit_store
from _validator.core.request import Request
from execution_layer.circuit import ProofSystem, Circuit
from _validator.models.request_type import RequestType


@dataclass
class MinerResponse:
    """
    Represents a response from a miner.

    Attributes:
        uid (int): Unique identifier of the miner.
        verification_result (bool): Whether the miner's response was verified.
        response_time (float): Time taken by the miner to respond.
        verification_time (float): Time taken to verify the proof.
        proof_size (int): Size of the proof provided by the miner.
        circuit (Circuit): Circuit used.
        proof_content (Any): Content of the proof - either a string or a dict.
        raw (str): Deserialized form of the response.
        error (str): Error message, if any occurred during processing.
    """

    uid: int
    verification_result: bool
    input_hash: str
    response_time: float
    proof_size: int
    circuit: Circuit
    verification_time: float | None = None
    proof_content: dict | str | None = None
    public_json: list[str] | None = None
    request_type: RequestType | None = None
    raw: dict | None = None
    error: str | None = None
    save: bool = False

    @classmethod
    def from_raw_response(cls, response: Request) -> "MinerResponse":
        """
        Creates a MinerResponse object from a raw response dictionary.

        Args:
            response (dict): Raw response from a miner.

        Returns:
            MinerResponse: Processed miner response object.
        """
        try:
            deserialized_response = response.deserialized
            bt.logging.trace(f"Deserialized response: {deserialized_response}")
            proof_content = None
            public_json = None
            if isinstance(deserialized_response, str):
                try:
                    deserialized_response = json.loads(deserialized_response)
                except json.JSONDecodeError as e:
                    bt.logging.debug(f"JSON decoding error: {e}")
                    return cls.empty(uid=response.uid, circuit=response.circuit)

            if isinstance(deserialized_response, dict):
                proof = deserialized_response.get("proof", "{}")
                public_signals = deserialized_response.get("public_signals", "[]")

                if isinstance(proof, str):
                    if all(c in "0123456789ABCDEFabcdef" for c in proof):
                        proof_content = proof
                    else:
                        proof_content = json.loads(proof)
                else:
                    proof_content = proof
                if public_signals and str(public_signals).strip():
                    public_json = (
                        json.loads(public_signals)
                        if isinstance(public_signals, str)
                        else public_signals
                    )
                else:
                    bt.logging.debug(
                        f"Miner at {response.uid} did not return public signals."
                    )

            if isinstance(proof_content, str):
                proof_size = len(proof_content)
            else:
                if response.circuit.proof_system == ProofSystem.CIRCOM:
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
                elif response.circuit.proof_system == ProofSystem.EZKL:
                    proof_size = len(proof_content["proof"])
                else:
                    proof_size = DEFAULT_PROOF_SIZE

            return cls(
                uid=response.uid,
                verification_result=False,
                response_time=response.response_time,
                proof_size=proof_size or DEFAULT_PROOF_SIZE,
                circuit=response.circuit,
                proof_content=proof_content,
                request_type=response.request_type,
                input_hash=response.request_hash,
                public_json=public_json,
                raw=deserialized_response,
                save=response.save,
            )
        except json.JSONDecodeError as e:
            traceback.print_exc()
            bt.logging.error(f"JSON decoding error: {e}")
            return cls.empty(uid=response.uid, circuit=response.circuit)
        except Exception as e:
            traceback.print_exc()
            bt.logging.error(f"Error processing miner response: {e}")
            return cls.empty(uid=response.uid, circuit=response.circuit)

    @classmethod
    def empty(cls, uid: int = 0, circuit: Circuit | None = None) -> "MinerResponse":
        """
        Creates an empty MinerResponse object.

        Returns:
            MinerResponse: An empty MinerResponse object.
        """
        if circuit is None:
            circuit = circuit_store.get_circuit(SINGLE_PROOF_OF_WEIGHTS_MODEL_ID)
        timeout = (
            circuit.timeout if circuit and circuit.timeout else CIRCUIT_TIMEOUT_SECONDS
        )
        return cls(
            uid=uid,
            verification_result=False,
            response_time=timeout,
            verification_time=None,
            proof_size=DEFAULT_PROOF_SIZE,
            circuit=circuit,
            proof_content=None,
            public_json=None,
            request_type=RequestType.BENCHMARK,
            input_hash=None,
            raw=None,
            error="Empty response",
            save=False,
        )

    def to_log_dict(self, metagraph: bt.metagraph) -> dict:  # type: ignore
        """
        Parse a MinerResponse object into a dictionary.
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
            "input_hash": self.input_hash,
            "request_type": self.request_type.value,
            "error": self.error,
            "save": self.save,
        }

    def set_verification_result(self, result: bool):
        """
        Sets the verification result for the miner's response.

        Args:
            result (bool): The verification result to set.
        """
        self.verification_result = result

    def __iter__(self):
        return iter(self.__dict__.items())
