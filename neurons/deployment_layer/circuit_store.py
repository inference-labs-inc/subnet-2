from __future__ import annotations

import os
import traceback
from typing import Optional

import bittensor as bt
from packaging import version

from constants import IGNORED_MODEL_HASHES, MAINNET_TESTNET_UIDS
from execution_layer.circuit import Circuit


class CircuitStore:
    """
    A Singleton class to manage and store Circuit objects.

    This class is responsible for loading, storing, and retrieving Circuit objects.
    """

    _instance = None

    def __new__(cls, *args, **kwargs):
        """
        Override the __new__ method to implement the Singleton pattern.
        """
        if not cls._instance:
            cls._instance = super(CircuitStore, cls).__new__(cls, *args, **kwargs)
        return cls._instance

    def __init__(self):
        """
        Initialize the CircuitStore.

        Creates an empty dictionary to store Circuit objects and loads circuits.
        """
        self.circuits: dict[str, Circuit] = {}

    def load_circuits(self, deployment_layer_path: Optional[str] = None):
        """
        Load circuits from the file system.

        Searches for directories starting with 'model_' in the deployment layer path,
        attempts to create Circuit objects from these directories, and stores them
        in the circuits dictionary.
        """
        deployment_layer_path = os.path.dirname(__file__)
        bt.logging.info(f"Loading circuits from {deployment_layer_path}")

        for folder_name in os.listdir(deployment_layer_path):
            folder_path = os.path.join(deployment_layer_path, folder_name)

            if os.path.isdir(folder_path) and folder_name.startswith("model_"):
                circuit_id = folder_name.split("_")[1]

                if circuit_id in IGNORED_MODEL_HASHES:
                    bt.logging.info(f"Ignoring circuit {circuit_id}")
                    continue

                metadata_path = os.path.join(folder_path, "metadata.json")
                if not os.path.isfile(metadata_path):
                    bt.logging.debug(f"Skipping {folder_name} - no metadata.json")
                    continue

                try:
                    bt.logging.debug(f"Attempting to load circuit {circuit_id}")
                    circuit = Circuit(circuit_id)
                    self.circuits[circuit_id] = circuit
                    bt.logging.info(f"Successfully loaded circuit {circuit_id}")
                except Exception as e:
                    bt.logging.error(f"Error loading circuit {circuit_id}: {e}")
                    traceback.print_exc()
                    continue

        bt.logging.info(f"Loaded {len(self.circuits)} circuits")

    def get_circuit(self, circuit_id: str) -> Circuit | None:
        """
        Retrieve a Circuit object by its ID.

        Args:
            circuit_id (str): The ID of the circuit to retrieve.

        Returns:
            Circuit | None: The Circuit object if found, None otherwise.
        """
        circuit = self.circuits.get(circuit_id)
        if circuit:
            bt.logging.debug(f"Retrieved circuit {circuit}")
        else:
            bt.logging.warning(f"Circuit {circuit_id} not found")
        return circuit

    def get_latest_circuit_for_netuid(self, netuid: int):
        """
        Get the latest circuit for a given netuid by comparing semver version strings.

        Args:
            netuid (int): The subnet ID to find the latest circuit for

        Returns:
            Circuit | None: The circuit with the highest semver version for the given netuid,
            or None if no circuits found
        """

        matching_circuits = [
            c for c in self.circuits.values() if c.metadata.netuid == netuid
        ]
        if not matching_circuits:
            return None

        return max(matching_circuits, key=lambda c: version.parse(c.metadata.version))

    def get_circuit_for_netuid_and_version(
        self, netuid: int, version: int
    ) -> Circuit | None:
        """
        Get the circuit for a given netuid and version.
        """
        matching_circuits = [
            c
            for c in self.circuits.values()
            if c.metadata.netuid == netuid and c.metadata.weights_version == version
        ]
        if not matching_circuits:
            bt.logging.warning(
                f"No circuit found for netuid {netuid} and weights version {version}"
            )
            return None
        return matching_circuits[0]

    def get_latest_circuit_by_name(self, circuit_name: str) -> Circuit | None:
        """
        Get the latest circuit by name.
        """
        matching_circuits = [
            c for c in self.circuits.values() if c.metadata.name == circuit_name
        ]
        return max(matching_circuits, key=lambda c: version.parse(c.metadata.version))

    def get_circuit_by_name_and_version(
        self, circuit_name: str, version: int
    ) -> Circuit | None:
        """
        Get the circuit by name and version.
        """
        matching_circuits = [
            c
            for c in self.circuits.values()
            if c.metadata.name == circuit_name and c.metadata.version == version
        ]
        return matching_circuits[0] if matching_circuits else None

    def list_circuits(self) -> list[str]:
        """
        Get a list of all circuit IDs.

        Returns:
            list[str]: A list of circuit IDs.
        """
        circuit_list = list(self.circuits.keys())
        bt.logging.debug(f"Listed {len(circuit_list)} circuits")
        return circuit_list

    def list_circuit_metadata(self) -> list[dict]:
        """
        JSON safe circuit metadata for use in API serving.
        """
        data: list[dict] = []
        for circuit in self.circuits.values():
            data.append(
                {
                    "id": circuit.id,
                    "name": circuit.metadata.name,
                    "description": circuit.metadata.description,
                    "author": circuit.metadata.author,
                    "version": circuit.metadata.version,
                    "type": circuit.metadata.type,
                    "proof_system": circuit.metadata.proof_system,
                    "netuid": circuit.metadata.netuid,
                    "testnet_netuids": (
                        [
                            uid[1]
                            for uid in MAINNET_TESTNET_UIDS
                            if uid[0] == int(circuit.metadata.netuid)
                        ]
                        if circuit.metadata.netuid
                        else None
                    ),
                    "weights_version": circuit.metadata.weights_version,
                    "input_schema": circuit.input_handler.schema.model_json_schema(),
                }
            )
        return data


circuit_store = CircuitStore()
bt.logging.info("CircuitStore initialized")
