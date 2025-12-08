import json
import os
import subprocess
import traceback
from typing import TYPE_CHECKING
import bittensor as bt
from execution_layer.proof_handlers.base_handler import ProofSystemHandler
from execution_layer.generic_input import GenericInput

if TYPE_CHECKING:
    from execution_layer.verified_model_session import VerifiedModelSession

# Custom home directory for Jolt
JOLT_HOME = os.path.join(os.path.expanduser("~"), ".jolt_home")

if not os.path.exists(JOLT_HOME):
    os.makedirs(JOLT_HOME)


class JoltHandler(ProofSystemHandler):
    def gen_input_file(self, session):
        bt.logging.trace("Generating input file")
        data = session.inputs.to_json()
        dir_name = os.path.dirname(session.session_storage.input_path)
        os.makedirs(dir_name, exist_ok=True)
        with open(session.session_storage.input_path, "w", encoding="utf-8") as f:
            json.dump(data, f)
        bt.logging.trace(f"Generated input.json with data: {data}")

    def generate_witness(
        self, session: "VerifiedModelSession", return_content: bool = False
    ) -> None:
        raise NotImplementedError("JoltHandler does not implement generate_witness")

    def gen_proof(self, session):
        try:
            bt.logging.debug(
                f"Starting proof generation with paths: {session.session_storage.input_path}, "
                f"{session.model.paths.compiled_model}, {session.session_storage.proof_path}, "
                f"{session.session_storage.public_path}"
            )
            proof, out = self.proof_worker(
                input_path=session.session_storage.input_path,
                circuit_path=session.model.paths.compiled_model,
                proof_path=session.session_storage.proof_path,
                public_path=session.session_storage.public_path,
            )
            return proof, out
        except Exception as e:
            bt.logging.error(f"An error occurred during proof generation: {e}")
            raise

    def verify_proof(
        self,
        session: "VerifiedModelSession",
        public_data: GenericInput,
        proof: str,
    ) -> bool:
        try:
            proof_bytes = bytes.fromhex(proof)
            with open(session.session_storage.proof_path, "wb") as proof_file:
                proof_file.write(proof_bytes)

            with open(
                session.session_storage.public_path, "w", encoding="utf-8"
            ) as public_file:
                json.dump(public_data.to_json(), public_file)

            result = subprocess.run(
                [
                    session.model.paths.compiled_model,
                    "verify",
                    "--input",
                    session.session_storage.input_path,
                    "--proof",
                    session.session_storage.proof_path,
                    "--output",
                    session.session_storage.public_path,
                ],
                check=True,
                capture_output=True,
                text=True,
                cwd=os.path.dirname(session.model.paths.compiled_model),
            )
            bt.logging.trace(f"Proof verification stdout: {result.stdout}")
            bt.logging.trace(f"Proof verification stderr: {result.stderr}")
            return result.returncode == 0
        except subprocess.CalledProcessError as e:
            bt.logging.error(f"Proof verification failed: {e}")
            bt.logging.error(f"Proof verification stdout: {e.stdout}")
            bt.logging.error(f"Proof verification stderr: {e.stderr}")
            return False
        except Exception as e:
            bt.logging.error(f"Unexpected error during proof verification: {e}")
            bt.logging.error(f"Error traceback: {traceback.format_exc()}")
            return False

    @staticmethod
    def proof_worker(
        input_path, circuit_path, proof_path, public_path
    ) -> tuple[bytes, str]:
        try:
            result = subprocess.run(
                [
                    circuit_path,
                    "prove",
                    "--input",
                    input_path,
                    "--output",
                    public_path,
                    "--proof",
                    proof_path,
                ],
                check=True,
                capture_output=True,
                text=True,
                cwd=os.path.dirname(circuit_path),
            )
            bt.logging.debug(f"Proof generated: {proof_path}")
            bt.logging.trace(f"Proof generation stdout: {result.stdout}")
            bt.logging.trace(f"Proof generation stderr: {result.stderr}")
            with open(proof_path, "rb") as proof_file:
                proof = proof_file.read()
            with open(public_path, "r", encoding="utf-8") as public_file:
                public_data = public_file.read()
            return proof, public_data
        except subprocess.CalledProcessError as e:
            bt.logging.error(f"Error generating proof: {e}")
            bt.logging.error(f"Proof generation stdout: {e.stdout}")
            bt.logging.error(f"Proof generation stderr: {e.stderr}")
            raise

    @staticmethod
    def aggregate_proofs(
        session: "VerifiedModelSession", proofs: list[str]
    ) -> tuple[str, float]:
        raise NotImplementedError(
            "Aggregation of proofs is not implemented for JoltHandler"
        )
