from __future__ import annotations

import os
from typing import Any, ClassVar

import bittensor as bt
import toml
from execution_layer.circuit import ProofSystem
from pydantic import BaseModel


class QueryZkProof(BaseModel):
    """
    Data model for querying zk proofs.
    """

    name: ClassVar = "query-zk-proof"

    # Required request input, filled by caller.
    model_id: str | None = None
    query_input: Any | None = None

    # Optional request output, filled by receiving miner.
    query_output: str | None = None

    def deserialize(self: QueryZkProof) -> str | None:
        """
        unpack query_output
        """
        return self.query_output


class ProofOfWeightsDataModel(BaseModel):
    """
    Data model for conveying proof of weights messages
    """

    name: ClassVar = "proof-of-weights"
    subnet_uid: int = 2
    verification_key_hash: str
    proof_system: ProofSystem = ProofSystem.CIRCOM
    inputs: dict
    proof: str
    public_signals: str

    def deserialize(self) -> dict | None:
        """
        Return the proof and input data
        """
        return {
            "inputs": self.inputs,
            "proof": self.proof,
            "public_signals": self.public_signals,
        }


class Competition(BaseModel):
    """
    A synapse for conveying competition messages and circuit files
    """

    name: ClassVar = "competition"
    id: int  # Competition ID
    hash: str  # Circuit hash
    file_name: str  # Name of file being requested
    file_content: str | None = None  # Hex encoded file content
    commitment: str | None = None  # Circuit commitment data from miner
    error: str | None = None  # Error message if something goes wrong

    def deserialize(self) -> dict:
        """Return all fields including required ones"""
        return {
            "id": self.id,
            "hash": self.hash,
            "file_name": self.file_name,
            "file_content": self.file_content,
            "commitment": self.commitment,
            "error": self.error,
        }


# Note these are going to need to change to lighting.Synapse
class QueryForCapacities(BaseModel):
    """
    Query for capacities allocated to each circuit
    """

    name: ClassVar = "capacities"
    capacities: dict[str, int] | None = None

    def deserialize(self) -> dict[str, int] | None:
        """
        Return the capacities
        """
        return self.capacities

    @staticmethod
    def from_config(config_path: str | None = None) -> dict[str, int]:
        if config_path is None:
            # Use env var if available, otherwise fall back to default config path
            config_path = os.environ.get("MINER_CIRCUITS_CONFIG", "miner.config.toml")
        try:
            with open(config_path, "r") as f:
                config = toml.load(f)
                circuits = config.get("miner", {}).get("circuits", [])
                return {
                    circuit.get("id"): circuit.get("compute_units", 0)
                    for circuit in circuits
                    if "id" in circuit
                }
        except Exception as e:
            bt.logging.error(f"Error loading capacities from config: {e}")
            return {}


class DSliceProofGenerationDataModel(BaseModel):
    """
    Data model for conveying DSPERSE proof generation messages
    """

    name: ClassVar = "dsperse-proof-generation"
    circuit: str | None = None
    proof_system: ProofSystem = ProofSystem.JSTPROVE
    inputs: Any | None = None
    outputs: Any | None = None
    slice_num: str | None = None
    run_uid: str | None = None
