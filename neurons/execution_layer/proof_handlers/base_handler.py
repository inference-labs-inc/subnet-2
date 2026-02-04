from __future__ import annotations
import json
import os
from abc import ABC, abstractmethod
from typing import TYPE_CHECKING

import bittensor as bt
from execution_layer.base_input import BaseInput

if TYPE_CHECKING:
    from execution_layer.verified_model_session import VerifiedModelSession


class ProofSystemHandler(ABC):
    """
    An abstract base class for proof system handlers.
    """

    def gen_input_file(self, session: VerifiedModelSession):
        bt.logging.trace("Generating input file")
        data = session.inputs.to_json()
        os.makedirs(os.path.dirname(session.session_storage.input_path), exist_ok=True)
        with open(session.session_storage.input_path, "w", encoding="utf-8") as f:
            json.dump(data, f)
        bt.logging.trace(f"Generated input.json with data: {data}")

    @abstractmethod
    def gen_proof(self, session: VerifiedModelSession) -> tuple[str, str]:
        """
        Generate a proof for the given session.

        Args:
            session (VerifiedModelSession): The current handler session.

        Returns:
            tuple[str, str]: A tuple containing the proof content (str),
            the public data (str).
        """

    @abstractmethod
    def verify_proof(
        self,
        session: VerifiedModelSession,
        validator_inputs: BaseInput,
        proof: dict | str,
    ) -> bool:
        """
        Verify a proof for the given session.

        Args:
            session (VerifiedModelSession): The current handler session.
            validator_inputs (BaseInput): The validator inputs to verify the proof against.
            proof (dict | str): The proof to verify.
        """

    @abstractmethod
    def generate_witness(
        self, session: VerifiedModelSession, return_content: bool = False
    ) -> list | dict:
        """
        Generate a witness for the given session.

        Args:
            session (VerifiedModelSession): The current handler session.
            return_content (bool): Whether to return the witness content.
        """
