from __future__ import annotations

import json
import os
import traceback
from typing import Optional

import bittensor as bt
import httpx
from packaging import version

from constants import (
    CIRCUIT_API_URL,
    CIRCUIT_CACHE_DIR,
    CIRCUIT_METADATA_FILENAME,
    IGNORED_MODEL_HASHES,
    MAINNET_TESTNET_UIDS,
)
from execution_layer.circuit import Circuit, CircuitMetadata


class CircuitStore:
    _instance = None

    def __new__(cls, *args, **kwargs):
        if not cls._instance:
            cls._instance = super(CircuitStore, cls).__new__(cls, *args, **kwargs)
            cls._instance._initialized = False
        return cls._instance

    def __init__(self):
        if self._initialized:
            return
        self.circuits: dict[str, Circuit] = {}
        self._api_url = CIRCUIT_API_URL
        self._cache_dir = CIRCUIT_CACHE_DIR
        self._initialized = True

    def load_circuits(self, deployment_layer_path: Optional[str] = None):
        bt.logging.info("Loading circuits...")

        active_ids: set[str] | None = None
        api_data: list[dict] | None = None
        bt.logging.info("Fetching active circuits from API...")
        try:
            api_data = self._fetch_circuits_from_api()
            active_ids = {c["id"] for c in api_data if c.get("id")}
            bt.logging.info(f"API reports {len(active_ids)} active circuits")
        except Exception as e:
            bt.logging.warning(f"Failed to fetch active circuits from API: {e}")

        self._load_from_filesystem(deployment_layer_path, active_ids)
        self._load_from_cache(active_ids)

        if api_data is not None:
            self._load_from_api(api_data)

        bt.logging.info(f"Loaded {len(self.circuits)} circuits")

    def _fetch_circuits_from_api(self) -> list[dict]:
        with httpx.Client(timeout=30) as client:
            response = client.get(f"{self._api_url}/circuits")
            response.raise_for_status()
            data = response.json()
        return data.get("circuits", [])

    def _iter_circuit_dirs(
        self, base_path: str, active_ids: set[str] | None, source: str
    ):
        for folder_name in os.listdir(base_path):
            folder_path = os.path.join(base_path, folder_name)
            if not os.path.isdir(folder_path) or not folder_name.startswith("model_"):
                continue
            circuit_id = folder_name[6:]
            if circuit_id in IGNORED_MODEL_HASHES:
                bt.logging.debug(f"Ignoring {source} circuit {circuit_id}")
                continue
            if active_ids is not None and circuit_id not in active_ids:
                bt.logging.debug(
                    f"Skipping {source} circuit {circuit_id} - not in active list"
                )
                continue
            if circuit_id in self.circuits:
                continue
            metadata_path = os.path.join(folder_path, CIRCUIT_METADATA_FILENAME)
            if not os.path.isfile(metadata_path):
                bt.logging.debug(
                    f"Skipping {folder_name} - no {CIRCUIT_METADATA_FILENAME}"
                )
                continue
            yield circuit_id, folder_path, metadata_path

    def _load_from_cache(self, active_ids: set[str] | None = None):
        if not os.path.exists(self._cache_dir):
            return

        bt.logging.info(f"Loading circuits from cache: {self._cache_dir}")

        for circuit_id, folder_path, metadata_path in self._iter_circuit_dirs(
            self._cache_dir, active_ids, "cached"
        ):
            try:
                with open(metadata_path, "r", encoding="utf-8") as f:
                    metadata_dict = json.load(f)
                if not metadata_dict.pop("complete", True):
                    bt.logging.warning(
                        f"Skipping incomplete cached circuit {circuit_id}"
                    )
                    continue
                metadata = CircuitMetadata.from_dict(metadata_dict)
                circuit = Circuit(circuit_id, metadata=metadata)
                self.circuits[circuit_id] = circuit
                bt.logging.info(f"Loaded circuit {circuit_id} from cache")
            except Exception as e:
                bt.logging.error(f"Error loading cached circuit {circuit_id}: {e}")
                traceback.print_exc()

    def _load_from_api(self, circuits_data: list[dict]):
        bt.logging.info(f"Fetching circuits from {self._api_url}")
        bt.logging.info(f"Found {len(circuits_data)} circuits from API")

        for circuit_data in circuits_data:
            circuit_id = circuit_data.get("id")
            if not circuit_id:
                continue

            if circuit_id in IGNORED_MODEL_HASHES:
                bt.logging.debug(f"Ignoring circuit {circuit_id}")
                continue

            if circuit_id in self.circuits:
                continue

            try:
                self._cache_circuit(circuit_id, circuit_data)
                metadata = CircuitMetadata.from_dict(circuit_data.get("metadata", {}))
                circuit = Circuit(circuit_id, metadata=metadata)
                self.circuits[circuit_id] = circuit
                bt.logging.info(f"Loaded circuit {circuit_id} from API")
            except Exception as e:
                bt.logging.error(f"Error loading circuit {circuit_id} from API: {e}")
                traceback.print_exc()

    def _cache_circuit(self, circuit_id: str, circuit_data: dict):
        cache_path = os.path.join(self._cache_dir, f"model_{circuit_id}")
        os.makedirs(cache_path, exist_ok=True)

        metadata = circuit_data.get("metadata", {})
        critical_files = set(metadata.get("critical_files", []))

        files = circuit_data.get("files", {})
        failed_downloads = []
        for filename, url in files.items():
            if filename == "metadata.json":
                continue
            try:
                self._download_file(url, os.path.join(cache_path, filename))
            except Exception as e:
                bt.logging.warning(
                    f"Failed to download {filename} for circuit {circuit_id}: {e}"
                )
                failed_downloads.append(filename)

        failed_critical = critical_files & set(failed_downloads)
        complete = len(failed_critical) == 0

        metadata["complete"] = complete
        metadata_path = os.path.join(cache_path, CIRCUIT_METADATA_FILENAME)
        with open(metadata_path, "w", encoding="utf-8") as f:
            json.dump(metadata, f, indent=2)

        if failed_downloads:
            bt.logging.warning(
                f"Circuit {circuit_id}: {len(failed_downloads)}/{len(files)} files failed to download"
            )

        if failed_critical:
            raise RuntimeError(
                f"Circuit {circuit_id} missing critical files: {', '.join(sorted(failed_critical))}"
            )

    def _download_file(self, url: str, dest_path: str):
        if os.path.exists(dest_path):
            return

        bt.logging.debug(f"Downloading {url} to {dest_path}")
        try:
            with httpx.Client(timeout=httpx.Timeout(30.0, read=300.0)) as client:
                with client.stream("GET", url) as response:
                    response.raise_for_status()
                    with open(dest_path, "wb") as f:
                        for chunk in response.iter_bytes(chunk_size=65536):
                            f.write(chunk)
        except Exception:
            if os.path.exists(dest_path):
                os.remove(dest_path)
            raise

    def _load_from_filesystem(
        self,
        deployment_layer_path: Optional[str] = None,
        active_ids: set[str] | None = None,
    ):
        if deployment_layer_path is None:
            deployment_layer_path = os.path.dirname(__file__)

        bt.logging.info(f"Loading circuits from {deployment_layer_path}")

        if not os.path.exists(deployment_layer_path):
            return

        for circuit_id, _, _ in self._iter_circuit_dirs(
            deployment_layer_path, active_ids, "filesystem"
        ):
            try:
                circuit = Circuit(circuit_id)
                self.circuits[circuit_id] = circuit
                bt.logging.info(f"Loaded circuit {circuit_id} from filesystem")
            except Exception as e:
                bt.logging.error(f"Error loading circuit {circuit_id}: {e}")
                traceback.print_exc()

    def refresh_circuits(self):
        try:
            circuits_data = self._fetch_circuits_from_api()
            existing_ids = set(self.circuits.keys())
            self._load_from_api(circuits_data)
            new_ids = set(self.circuits.keys()) - existing_ids
            for circuit_id in new_ids:
                circuit = self.circuits[circuit_id]
                file_count = len(circuits_data)
                for cd in circuits_data:
                    if cd.get("id") == circuit_id:
                        file_count = len(cd.get("files", {}))
                        break
                bt.logging.info(
                    f"Found new circuit: {circuit.metadata.name} v{circuit.metadata.version} ({file_count} files)"
                )
        except Exception as e:
            bt.logging.warning(f"Failed to refresh circuits from API: {e}")

    def ensure_circuit(self, circuit_id: str) -> Circuit:
        if circuit_id in self.circuits:
            return self.circuits[circuit_id]

        if circuit_id in IGNORED_MODEL_HASHES:
            raise ValueError(f"Circuit {circuit_id} is in the ignored list")

        bt.logging.info(f"Circuit {circuit_id} not loaded, fetching from API...")
        with httpx.Client(timeout=30) as client:
            response = client.get(f"{self._api_url}/circuits/{circuit_id}")
            response.raise_for_status()
            circuit_data = response.json()

        self._cache_circuit(circuit_id, circuit_data)
        metadata = CircuitMetadata.from_dict(circuit_data.get("metadata", {}))
        circuit = Circuit(circuit_id, metadata=metadata)
        self.circuits[circuit_id] = circuit
        bt.logging.success(f"Fetched and loaded circuit {circuit_id}")
        return circuit

    def get_latest_circuit_for_netuid(self, netuid: int):
        matching_circuits = [
            c for c in self.circuits.values() if c.metadata.netuid == netuid
        ]
        if not matching_circuits:
            return None

        return max(matching_circuits, key=lambda c: version.parse(c.metadata.version))

    def get_circuit_for_netuid_and_version(
        self, netuid: int, version: int
    ) -> Circuit | None:
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
        matching_circuits = [
            c for c in self.circuits.values() if c.metadata.name == circuit_name
        ]
        if not matching_circuits:
            return None
        return max(matching_circuits, key=lambda c: version.parse(c.metadata.version))

    def get_circuit_by_name_and_version(
        self, circuit_name: str, version: int
    ) -> Circuit | None:
        matching_circuits = [
            c
            for c in self.circuits.values()
            if c.metadata.name == circuit_name and c.metadata.version == version
        ]
        return matching_circuits[0] if matching_circuits else None

    def list_circuits(self) -> list[str]:
        circuit_list = list(self.circuits.keys())
        bt.logging.debug(f"Listed {len(circuit_list)} circuits")
        return circuit_list

    def list_circuit_metadata(self) -> list[dict]:
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
