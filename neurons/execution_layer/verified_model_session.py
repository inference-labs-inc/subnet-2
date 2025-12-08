from __future__ import annotations
import asyncio
import multiprocessing

import os
import time
import traceback
import uuid

import bittensor as bt
from attr import define, field
from execution_layer.circuit import Circuit
from execution_layer.proof_handlers.base_handler import ProofSystemHandler
from execution_layer.proof_handlers.factory import ProofSystemFactory
from execution_layer.session_storage import SessionStorage
from execution_layer.base_input import BaseInput
from execution_layer.generic_input import GenericInput

# Ensure new processes do not copy the main process
multiprocessing.set_start_method("fork", force=True)


@define
class VerifiedModelSession:
    """
    Represents a session for a verified model execution.

    This class encapsulates the necessary components and operations for running
    and verifying a model within a proof system. It handles input preparation,
    proof generation, and verification. It facilitates interaction with multiple
    proof systems and circuits in a consistent manner.

    Attributes:
        model (Circuit): The circuit representation of the model.
        session_storage (SessionStorage): Manages storage for session-related files.
        inputs (list[float | list[float]]): Input data for the model.
        session_id (str): Unique identifier for the session.
        proof_handler (ProofSystemHandler): Handles proof-related operations.

    """

    model: Circuit = field()
    session_storage: SessionStorage = field(init=False)
    inputs: BaseInput | None = field(factory=None)
    session_id: str = field(init=False, factory=lambda: str(uuid.uuid4()))
    proof_handler: ProofSystemHandler = field(init=False)

    def __init__(
        self,
        inputs: BaseInput | None = None,
        model: Circuit | None = None,
    ):
        if model is None:
            raise ValueError("Model must be provided")
        self.model = model
        self.inputs = inputs
        self.session_id = str(uuid.uuid4())
        self.session_storage = SessionStorage(self.model.id, self.session_id)
        self.proof_handler = ProofSystemFactory.get_handler(
            self.model.metadata.proof_system
        )
        self.gen_input_file()

    def gen_input_file(self):
        """
        Generate an input file for use in witness creation.
        """
        self.proof_handler.gen_input_file(self)

    def gen_proof(self) -> tuple[str, str, float]:
        """
        Generate a proof for a given inference.
        """
        try:
            bt.logging.debug("Starting proof generation process...")
            start_time = time.time()

            with multiprocessing.Pool(1) as p:
                proof_content = p.apply(
                    func=self._proof_worker,
                    args=[self],
                )

            proof_time = time.time() - start_time
            bt.logging.info(f"Proof generation took {proof_time} seconds")
            bt.logging.trace(f"Proof content: {proof_content}")
            return proof_content[0], proof_content[1], proof_time

        except Exception as e:
            bt.logging.error(f"An error occurred during proof generation: {e}")
            traceback.print_exc()
            raise

    def aggregate_proofs(self, proofs: list[str]) -> tuple[str, float]:
        """
        Aggregate multiple proofs into a single proof.
        """
        return self.proof_handler.aggregate_proofs(self, proofs)

    @staticmethod
    def _proof_worker(session: VerifiedModelSession) -> tuple[str, str]:
        """
        Handle the proof generation process in a separate process.
        """
        bt.logging.debug("Starting proof_worker")
        loop = asyncio.new_event_loop()
        asyncio.set_event_loop(loop)
        try:
            result = loop.run_until_complete(VerifiedModelSession._proof_task(session))
            bt.logging.debug("proof_task completed successfully")
            return result
        except Exception as e:
            bt.logging.error(f"Error in proof_worker: {str(e)}")
            raise
        finally:
            loop.close()

    @staticmethod
    async def _proof_task(session: VerifiedModelSession) -> tuple[str, str]:
        """
        Asynchronous task for generating a proof.
        """
        return session.proof_handler.gen_proof(session)

    def verify_proof(self, validator_inputs: GenericInput, proof: dict | str) -> bool:
        """
        Verify a proven inference.
        """
        try:
            bt.logging.debug("Starting proof verification process...")
            with multiprocessing.Pool(1) as p:
                verification_result = p.apply(
                    func=self._verify_worker,
                    args=[self, validator_inputs, proof],
                )
            return verification_result

        except Exception as e:
            bt.logging.error(f"An error occurred during proof verification: {e}")
            traceback.print_exc()
            raise

    @staticmethod
    def _verify_worker(
        session: VerifiedModelSession,
        validator_inputs: GenericInput,
        proof: dict | str,
    ) -> bool:
        """
        Handle the proof verification process in a separate process.
        """
        bt.logging.debug("Starting verify_worker")
        return session.proof_handler.verify_proof(session, validator_inputs, proof)

    def generate_witness(self, return_content: bool = False) -> list | dict:
        """
        Generate a witness file for use in proof generation.
        This performs an inference through the circuitized model.
        """
        return self.proof_handler.generate_witness(self, return_content)

    def __enter__(self):
        return self

    def end(self):
        self.remove_temp_files()

    def remove_temp_files(self):
        for path in (
            self.session_storage.input_path,
            self.session_storage.witness_path,
            self.session_storage.proof_path,
            self.session_storage.public_path,
        ):
            if os.path.exists(path):
                os.remove(path)

    def __exit__(self, exc_type, exc_val, exc_tb):
        return None
