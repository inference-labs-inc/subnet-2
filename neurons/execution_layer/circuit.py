from __future__ import annotations
from dataclasses import dataclass, field
from enum import Enum
import torch
import os
import json
import cli_parser
from execution_layer.input_registry import InputRegistry
from execution_layer.base_input import BaseInput
from utils.metrics_logger import log_circuit_metrics
from utils.gc_logging import gc_log_eval_metrics
from constants import (
    MAX_EVALUATION_ITEMS,
    DEFAULT_PROOF_SIZE,
    MAXIMUM_SCORE_MEDIAN_SAMPLE,
    CIRCUIT_TIMEOUT_SECONDS,
    ONE_MINUTE,
)
from utils import with_rate_limit
import time
import re

# trunk-ignore(pylint/E0611)
from bittensor import logging, subtensor, Wallet


class CircuitType(str, Enum):
    """
    Enum representing the type of circuit.
    """

    PROOF_OF_WEIGHTS = "proof_of_weights"
    PROOF_OF_COMPUTATION = "proof_of_computation"
    DSPERSE_PROOF_GENERATION = "dsperse_proof_generation"


class ProofSystem(str, Enum):
    """
    Enum representing supported proof systems.
    """

    # ZK Proof Systems
    ZKML = "ZKML"
    CIRCOM = "CIRCOM"
    JOLT = "JOLT"
    EZKL = "EZKL"

    def __str__(self):
        return self.value

    @classmethod
    def _missing_(cls, value):
        if isinstance(value, str):
            return cls(value.upper())
        raise ValueError(f"Cannot convert {value} to {cls.__name__}")

    def to_json(self):
        return self.value

    @classmethod
    def from_json(cls, value):
        return cls(value)


@dataclass
class CircuitPaths:
    """
    Paths to all files for the provided model.
    """

    model_id: str
    base_path: str = field(init=False)
    input: str = field(init=False)
    metadata: str = field(init=False)
    compiled_model: str = field(init=False)
    pk: str = field(init=False)
    vk: str = field(init=False)
    settings: str = field(init=False)
    witness: str = field(init=False)
    proof: str = field(init=False)
    srs: str = field(init=False)
    witness_executable: str = field(init=False)

    def __post_init__(self):
        self.base_path = os.path.join(
            os.path.dirname(__file__),
            "..",
            "deployment_layer",
            f"model_{self.model_id}",
        )
        if hasattr(cli_parser, "config") and cli_parser.config.full_path_models:
            self.external_base_path = os.path.join(
                cli_parser.config.full_path_models,
                f"model_{self.model_id}",
            )
        else:
            self.external_base_path = os.path.join(
                os.path.expanduser("~"),
                ".bittensor",
                "subnet-2",
                "models",
                f"model_{self.model_id}",
            )
        self.input = os.path.join(self.base_path, "input.json")
        self.metadata = os.path.join(self.base_path, "metadata.json")
        self.compiled_model = os.path.join(self.base_path, "model.compiled")
        self.settings = os.path.join(self.base_path, "settings.json")
        self.witness = os.path.join(self.base_path, "witness.json")
        self.proof = os.path.join(self.base_path, "proof.json")
        self.witness_executable = os.path.join(self.base_path, "witness.js")
        self.pk = os.path.join(self.external_base_path, "circuit.zkey")
        self.vk = os.path.join(self.base_path, "verification_key.json")
        self.evaluation_data = os.path.join(
            self.external_base_path, "evaluation_data.json"
        )

    def set_proof_system_paths(self, proof_system: ProofSystem):
        """
        Set proof system-specific paths.
        """
        if proof_system == ProofSystem.CIRCOM:
            self.pk = os.path.join(self.external_base_path, "circuit.zkey")
            self.vk = os.path.join(self.base_path, "verification_key.json")
            self.compiled_model = os.path.join(self.base_path, "circuit.wasm")
        elif proof_system == ProofSystem.JOLT:
            self.compiled_model = os.path.join(
                self.base_path, "target", "release", "circuit"
            )
        elif proof_system == ProofSystem.EZKL:
            self.pk = os.path.join(self.external_base_path, "pk.key")
            self.vk = os.path.join(self.base_path, "vk.key")
            self.compiled_model = os.path.join(self.base_path, "model.compiled")
        else:
            raise ValueError(f"Proof system {proof_system} not supported")


@dataclass
class CircuitMetadata:
    """
    Metadata for a specific model, such as name, version, description, etc.
    """

    name: str
    description: str
    author: str
    version: str
    proof_system: str
    type: CircuitType
    external_files: None | dict[str, str]
    dslices: None | list[dict] = None
    netuid: int | None = None
    weights_version: int | None = None
    timeout: int | None = None
    benchmark_choice_weight: float | None = None

    @classmethod
    def from_file(cls, metadata_path: str) -> CircuitMetadata:
        """
        Create a ModelMetadata instance from a JSON file.

        Args:
            metadata_path (str): Path to the metadata JSON file.

        Returns:
            ModelMetadata: An instance of ModelMetadata.
        """
        with open(metadata_path, "r", encoding="utf-8") as f:
            metadata = json.load(f)
        return cls(**metadata)


@dataclass
class CircuitEvaluationItem:
    """
    Data collected from the evaluation of the circuit.
    """

    circuit: Circuit
    maximum_response_time: float
    minimum_response_time: float = 0.0
    uid: int = 0
    proof_size: int = DEFAULT_PROOF_SIZE
    response_time: float = 0.0
    score: float = 0.0
    verification_result: bool = False

    def __init__(self, *args, circuit: Circuit, **kwargs):

        self.circuit = circuit
        self.uid = kwargs.pop("uid", 0)
        self.minimum_response_time = kwargs.pop("minimum_response_time", 0.0)
        self.proof_size = kwargs.pop("proof_size", DEFAULT_PROOF_SIZE)
        self.response_time = kwargs.pop("response_time", 0.0)
        self.score = kwargs.pop("score", 0.0)
        self.verification_result = kwargs.pop("verification_result", False)

        self.maximum_response_time = (
            self.circuit.timeout
            if hasattr(self.circuit, "timeout") and self.circuit.timeout
            else CIRCUIT_TIMEOUT_SECONDS
        )

        for key, value in kwargs.items():
            setattr(self, key, value)

    def to_dict(self) -> dict:
        """Convert the evaluation item to a dictionary for JSON serialization."""
        return {
            "circuit_id": str(self.circuit.id),
            "uid": int(self.uid),
            "minimum_response_time": float(self.minimum_response_time),
            "maximum_response_time": float(self.maximum_response_time),
            "proof_size": int(self.proof_size),
            "response_time": float(self.response_time),
            "score": float(self.score),
            "verification_result": bool(self.verification_result),
        }


class CircuitEvaluationData:
    """
    Data collected from the evaluation of the circuit.
    """

    def __init__(self, circuit: Circuit, evaluation_store_path: str):
        self.circuit = circuit
        self.store_path = evaluation_store_path
        self.data: list[CircuitEvaluationItem] = []

        os.makedirs(os.path.dirname(evaluation_store_path), exist_ok=True)

        try:
            if os.path.exists(evaluation_store_path):
                with open(evaluation_store_path, "r", encoding="utf-8") as f:
                    data = json.load(f)
                    self.data = [
                        CircuitEvaluationItem(circuit=self.circuit, **item)
                        for item in data
                    ]
        except Exception as e:
            logging.error(
                f"Failed to load evaluation data for model {self.circuit.id}, starting fresh: {e}"
            )
            self.data = []

        if not self.data:
            with open(evaluation_store_path, "w", encoding="utf-8") as f:
                json.dump([], f)

    def update(self, item: CircuitEvaluationItem):
        """Update evaluation data, maintaining size limit."""
        for i, existing_item in enumerate(self.data):
            if existing_item.uid == item.uid:
                self.data[i] = item
                break
        else:
            self.data.append(item)

        if len(self.data) > MAX_EVALUATION_ITEMS:
            self.data = self.data[-MAX_EVALUATION_ITEMS:]

        try:
            with open(self.store_path, "w", encoding="utf-8") as f:
                json.dump([item.to_dict() for item in self.data], f)
        except Exception as e:
            logging.error(f"Failed to save evaluation data: {e}")

        self._log_metrics()

    @with_rate_limit(period=ONE_MINUTE)
    def _log_metrics(self) -> None:
        response_times = [r.response_time for r in self.data if r.verification_result]
        verified_count = len([r for r in self.data if r.verification_result])

        if response_times:
            log_circuit_metrics(response_times, verified_count, str(self.circuit))

            response_tensor = torch.tensor(response_times, dtype=torch.float32)
            mean_response_time = torch.mean(response_tensor).item()
            max_response_time = torch.max(response_tensor).item()
            min_response_time = torch.min(response_tensor).item()

            proof_sizes = torch.tensor(
                [r.proof_size for r in self.data if r.verification_result]
            )
            mean_proof_size = (
                torch.mean(proof_sizes, dtype=torch.float32).item()
                if len(proof_sizes) > 0
                else 0
            )
            sub = subtensor(config=cli_parser.config)
            last_block = sub.get_current_block()
            wallet = Wallet(config=cli_parser.config)

            gc_log_eval_metrics(
                model_id=self.circuit.id,
                model_name=self.circuit.metadata.name,
                netuid=self.circuit.metadata.netuid or 0,
                weights_version=self.circuit.metadata.weights_version or 0,
                proof_system=self.circuit.metadata.proof_system,
                circuit_type=str(self.circuit.metadata.type),
                proof_size=int(mean_proof_size),
                timeout=float(self.circuit.timeout),
                benchmark_weight=float(
                    self.circuit.metadata.benchmark_choice_weight or 0.0
                ),
                total_verifications=len(self.data),
                successful_verifications=verified_count,
                min_response_time=float(min_response_time),
                max_response_time=float(max_response_time),
                avg_response_time=float(mean_response_time),
                last_verification_time=int(time.time()),
                last_block=last_block,
                verification_ratio=self.verification_ratio,
                hotkey=wallet.hotkey,
            )

    @property
    def verification_ratio(self) -> float:
        """Get the ratio of successful verifications from recent evaluation data."""
        if not self.data:
            return 0.0

        successful = sum(1 for item in self.data if item.verification_result)
        return successful / len(self.data)

    def get_successful_response_times(self) -> list[float]:
        if not self.data:
            return []

        return sorted(
            r.response_time
            for r in self.data
            if r.verification_result and r.response_time > 0
        )

    @property
    def minimum_response_time(self) -> float:
        response_times = self.get_successful_response_times()

        if not response_times or len(response_times) in [0, 1]:
            return 0.0

        return torch.clamp(
            torch.min(torch.tensor(response_times)),
            0,
            self.circuit.timeout,
        ).item()

    @property
    def maximum_response_time(self) -> float:
        """Get maximum response time from evaluation data."""

        response_times = self.get_successful_response_times()
        if not response_times or len(response_times) in [0, 1]:
            return CIRCUIT_TIMEOUT_SECONDS

        sample_size = max(int(len(response_times) * MAXIMUM_SCORE_MEDIAN_SAMPLE), 1)

        return torch.clamp(
            torch.median(torch.tensor(response_times[-sample_size:])),
            0,
            self.circuit.timeout,
        ).item()


class Circuit:
    """
    A class representing a circuit.
    """

    def __init__(self, circuit_id: str):
        """
        Initialize a Model instance.

        Args:
            circuit_id (str): Unique identifier for the model.
        """

        if not circuit_id:
            raise ValueError("Circuit ID cannot be empty")

        if not re.match(r"^[a-f0-9]{64}$", circuit_id):
            raise ValueError(
                "Circuit ID must be a valid SHA-256 hash (64 lowercase hex characters)"
            )

        deployment_folder = os.path.join(
            os.path.dirname(__file__),
            "..",
            "deployment_layer",
            f"model_{circuit_id}",
        )

        if not os.path.exists(deployment_folder):
            raise ValueError(f"Circuit folder does not exist: model_{circuit_id}")

        if not os.path.isdir(deployment_folder):
            raise ValueError(f"Circuit path is not a directory: model_{circuit_id}")

        # XXX: might not fit to dsperse...
        self.paths = CircuitPaths(circuit_id)
        self.metadata = CircuitMetadata.from_file(self.paths.metadata)
        self.id = circuit_id
        self.proof_system = ProofSystem[self.metadata.proof_system]
        self.paths.set_proof_system_paths(self.proof_system)
        self.settings = {}
        self.evaluation_data = CircuitEvaluationData(self, self.paths.evaluation_data)
        self.timeout = (
            self.metadata.timeout
            if hasattr(self.metadata, "timeout") and self.metadata.timeout is not None
            else CIRCUIT_TIMEOUT_SECONDS
        )
        try:
            with open(self.paths.settings, "r", encoding="utf-8") as f:
                self.settings = json.load(f)
        except (FileNotFoundError, json.JSONDecodeError):
            logging.warning(
                f"Failed to load settings for model {self.id}. Using default settings."
            )
        self.input_handler: BaseInput = InputRegistry.get_handler(self.id)

    def __str__(self):
        return (
            f"{self.metadata.name} v{self.metadata.version} using {self.proof_system}"
        )
