from __future__ import annotations
import json
import os
from typing import TYPE_CHECKING
import subprocess
import bittensor as bt
import traceback
import ezkl
from enum import Enum

from execution_layer.proof_handlers.base_handler import ProofSystemHandler
from execution_layer.generic_input import GenericInput

if TYPE_CHECKING:
    from execution_layer.verified_model_session import VerifiedModelSession

LOCAL_EZKL_PATH = os.path.join(os.path.expanduser("~"), ".ezkl", "ezkl")


class EZKLInputType(Enum):
    F16 = ezkl.PyInputType.F16
    F32 = ezkl.PyInputType.F32
    F64 = ezkl.PyInputType.F64
    Int = ezkl.PyInputType.Int
    Bool = ezkl.PyInputType.Bool
    TDim = ezkl.PyInputType.TDim


class EZKLHandler(ProofSystemHandler):
    """
    Handler for the EZKL proof system.
    This class provides methods for generating and verifying proofs using EZKL.
    """

    def gen_input_file(self, session: VerifiedModelSession):
        bt.logging.trace("Generating input file")
        if isinstance(session.inputs.data, list):
            input_data = session.inputs.data
        else:
            input_data = session.inputs.to_array()
        data = {"input_data": input_data}
        os.makedirs(os.path.dirname(session.session_storage.input_path), exist_ok=True)
        with open(session.session_storage.input_path, "w", encoding="utf-8") as f:
            json.dump(data, f)
        bt.logging.trace(f"Generated input.json with data: {data}")

    def gen_proof(self, session: VerifiedModelSession) -> tuple[str, str]:
        try:
            bt.logging.debug("Starting proof generation...")

            self.generate_witness(session)
            bt.logging.trace("Generating proof")

            result = subprocess.run(
                [
                    LOCAL_EZKL_PATH,
                    "prove",
                    "--witness",
                    session.session_storage.witness_path,
                    "--compiled-circuit",
                    session.model.paths.compiled_model,
                    "--pk-path",
                    session.model.paths.pk,
                    "--proof-path",
                    session.session_storage.proof_path,
                ],
                check=True,
                capture_output=True,
                text=True,
            )

            bt.logging.trace(
                f"Proof generated: {session.session_storage.proof_path}, result: {result.stdout}"
            )

            with open(session.session_storage.proof_path, "r", encoding="utf-8") as f:
                proof = json.load(f)

            return json.dumps(proof), json.dumps(proof["instances"])

        except Exception as e:
            bt.logging.error(f"An error occurred during proof generation: {e}")
            traceback.print_exc()
            raise

    def verify_proof(
        self,
        session: VerifiedModelSession,
        validator_inputs: GenericInput,
        proof: str | dict,
    ) -> bool:
        if not proof:
            return False

        if isinstance(proof, str):
            proof_json = json.loads(proof)
        else:
            proof_json = proof

        input_instances = self.translate_inputs_to_instances(session, validator_inputs)

        proof_json["instances"] = [
            input_instances[:] + proof_json["instances"][0][len(input_instances) :]
        ]

        proof_json["transcript_type"] = "EVM"

        with open(session.session_storage.proof_path, "w", encoding="utf-8") as f:
            json.dump(proof_json, f)

        try:
            result = subprocess.run(
                [
                    LOCAL_EZKL_PATH,
                    "verify",
                    "--settings-path",
                    session.model.paths.settings,
                    "--proof-path",
                    session.session_storage.proof_path,
                    "--vk-path",
                    session.model.paths.vk,
                ],
                check=True,
                capture_output=True,
                text=True,
                timeout=60,
            )
            return "verified: true" in result.stdout
        except subprocess.TimeoutExpired:
            bt.logging.error("Verification process timed out after 60 seconds")
            return False
        except subprocess.CalledProcessError:
            return False

    def generate_witness(
        self, session: VerifiedModelSession, return_content: bool = False
    ) -> list | dict:
        bt.logging.trace("Generating witness")

        result = subprocess.run(
            [
                LOCAL_EZKL_PATH,
                "gen-witness",
                "--data",
                session.session_storage.input_path,
                "--compiled-circuit",
                session.model.paths.compiled_model,
                "--output",
                session.session_storage.witness_path,
                "--vk-path",
                session.model.paths.vk,
            ],
            check=True,
            capture_output=True,
            text=True,
        )

        bt.logging.debug(f"Gen witness result: {result.stdout}")

        if return_content:
            with open(session.session_storage.witness_path, "r", encoding="utf-8") as f:
                return json.load(f)
        return result.stdout

    def translate_inputs_to_instances(
        self, session: VerifiedModelSession, validator_inputs: GenericInput
    ) -> list[int]:
        scale_map = session.model.settings.get("model_input_scales", [])
        type_map = session.model.settings.get("input_types", [])
        return [
            ezkl.float_to_felt(x, scale_map[i], EZKLInputType[type_map[i]].value)
            for i, arr in enumerate(validator_inputs.to_array())
            for x in arr
        ]
