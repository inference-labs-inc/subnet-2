from __future__ import annotations
from typing import Dict, Optional

import bittensor as bt

import toml
from execution_layer.circuit import ProofSystem


class QueryZkProof(bt.Synapse):
    """
    QueryZkProof class inherits from bt.Synapse.
    It is used to query zkproof of certain model.
    """

    # Required request input, filled by sending dendrite caller.
    query_input: Optional[Dict] = None

    # Optional request output, filled by receiving axon.
    query_output: Optional[str] = None

    def deserialize(self: QueryZkProof) -> str | None:
        """
        unpack query_output
        """
        return self.query_output


class QueryForProvenInference(bt.Synapse):
    """
    A Synapse for querying proven inferences.
    DEV: This synapse is a placeholder.
    """

    query_input: Optional[dict] = None
    query_output: Optional[dict] = None

    def deserialize(self) -> dict | None:
        """
        Deserialize the query_output into a dictionary.
        """
        return self.query_output


class ProofOfWeightsSynapse(bt.Synapse):
    """
    A synapse for conveying proof of weights messages
    """

    subnet_uid: int = 2
    verification_key_hash: str
    proof_system: ProofSystem = ProofSystem.CIRCOM
    inputs: dict
    proof: str
    public_signals: str

    def deserialize(self) -> dict | None:
        """
        Return the proof
        """
        return {
            "inputs": self.inputs,
            "proof": self.proof,
            "public_signals": self.public_signals,
        }


class Competition(bt.Synapse):
    """
    A synapse for conveying competition messages and circuit files
    """

    id: int  # Competition ID
    hash: str  # Circuit hash
    file_name: str  # Name of file being requested
    file_content: Optional[str] = None  # Hex encoded file content
    commitment: Optional[str] = None  # Circuit commitment data from miner
    error: Optional[str] = None  # Error message if something goes wrong

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
class QueryForCapacities(bt.Synapse):
    """
    Query for capacities allocated to each circuit
    """

    capacities: Optional[dict[str, int]] = None

    def deserialize(self) -> dict[str, int]:
        """
        Return the capacities
        """
        return self.capacities

    @staticmethod
    def from_config(config_path: str = "miner.config.toml") -> dict[str, int]:
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
