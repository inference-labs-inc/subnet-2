from __future__ import annotations

import random
import secrets
from typing import Any

import numpy as np
from pydantic import BaseModel

from _validator.models.request_type import RequestType
from constants import ONE_MINUTE
from execution_layer.base_input import BaseInput


class TensorInputSchema(BaseModel):
    input_data: list


class PoWBatchInputSchema(BaseModel):
    maximum_score: list[float]
    previous_score: list[float]
    verified: list[bool]
    proof_size: list[float]
    response_time: list[float]
    competition: list[float]
    maximum_response_time: list[float]
    minimum_response_time: list[float]
    validator_uid: list[int]
    block_number: list[int]
    miner_uid: list[int]
    scaling: int
    RATE_OF_DECAY: int
    RATE_OF_RECOVERY: int
    FLATTENING_COEFFICIENT: int
    COMPETITION_WEIGHT: int
    PROOF_SIZE_WEIGHT: int
    PROOF_SIZE_THRESHOLD: int
    RESPONSE_TIME_WEIGHT: int
    MAXIMUM_RESPONSE_TIME_DECIMAL: int


def create_schema_from_metadata(input_schema: dict) -> type[BaseModel]:
    schema_type = input_schema.get("type")
    if schema_type == "tensor":
        return TensorInputSchema
    if schema_type == "pow_batch":
        return PoWBatchInputSchema
    raise ValueError(f"Unsupported input schema type: '{schema_type}'")


_POW_CONSTANT_DEFAULTS = {
    "rate_of_decay": 0.4,
    "rate_of_recovery": 0.1,
    "flattening_coefficient": 0.9,
    "proof_size_threshold": 3648,
    "proof_size_weight": 0,
    "response_time_weight": 1,
    "competition_weight": 0,
    "maximum_response_time_decimal": 0.99,
}


class GenericInputHandler(BaseInput):
    schema = TensorInputSchema

    def __init__(
        self,
        request_type: RequestType,
        data: dict[str, object] | None = None,
        input_schema: dict | None = None,
    ):
        self.input_schema = input_schema or {}
        if input_schema:
            self.schema = create_schema_from_metadata(input_schema)
        super().__init__(request_type, data)

    def _get_pow_constants(self) -> dict[str, float]:
        raw = self.input_schema.get("constants", {})
        return {k: raw.get(k, v) for k, v in _POW_CONSTANT_DEFAULTS.items()}

    def generate(self) -> dict[str, object]:
        schema_type = self.input_schema.get("type", "tensor")
        if schema_type == "pow_batch":
            return self._generate_pow_batch()

        shape = self.input_schema.get("shape", [1, 3, 224, 224])
        dtype = self.input_schema.get("dtype", "float32")
        normalization = self.input_schema.get("normalization")

        input_data = self._generate_tensor(shape, dtype, normalization)
        return {"input_data": input_data}

    def _generate_pow_batch(self) -> dict[str, object]:
        batch_size = self.input_schema.get("batch_size", 256)
        scaling = self.input_schema.get("scaling", 100000000)
        c = self._get_pow_constants()

        minimum_response_time = int(random.random() * ONE_MINUTE * scaling)
        maximum_response_time = minimum_response_time + int(
            random.random() * ONE_MINUTE * scaling
        )
        response_time = (
            int(random.random() * (maximum_response_time - minimum_response_time))
            + minimum_response_time
        )
        max_score = int(1 / 256 * scaling)

        return {
            "maximum_score": [max_score for _ in range(batch_size)],
            "previous_score": [
                int(random.random() * max_score) for _ in range(batch_size)
            ],
            "verified": [random.choice([True, False]) for _ in range(batch_size)],
            "proof_size": [
                int(random.randint(0, 5000) * scaling) for _ in range(batch_size)
            ],
            "validator_uid": [random.randint(0, 255) for _ in range(batch_size)],
            "block_number": [
                random.randint(3000000, 10000000) for _ in range(batch_size)
            ],
            "miner_uid": [random.randint(0, 255) for _ in range(batch_size)],
            "minimum_response_time": [minimum_response_time for _ in range(batch_size)],
            "maximum_response_time": [maximum_response_time for _ in range(batch_size)],
            "response_time": [response_time for _ in range(batch_size)],
            "competition": [int(random.random() * scaling) for _ in range(batch_size)],
            "scaling": scaling,
            "RATE_OF_DECAY": int(c["rate_of_decay"] * scaling),
            "RATE_OF_RECOVERY": int(c["rate_of_recovery"] * scaling),
            "FLATTENING_COEFFICIENT": int(c["flattening_coefficient"] * scaling),
            "PROOF_SIZE_WEIGHT": int(c["proof_size_weight"] * scaling),
            "PROOF_SIZE_THRESHOLD": int(c["proof_size_threshold"] * scaling),
            "COMPETITION_WEIGHT": int(c["competition_weight"] * scaling),
            "RESPONSE_TIME_WEIGHT": int(c["response_time_weight"] * scaling),
            "MAXIMUM_RESPONSE_TIME_DECIMAL": int(
                c["maximum_response_time_decimal"] * scaling
            ),
        }

    def _generate_tensor(
        self, shape: list[int], dtype: str, normalization: str | None
    ) -> list:
        try:
            np_dtype = np.dtype(dtype)
        except (TypeError, ValueError) as e:
            raise ValueError(f"Invalid dtype '{dtype}' from input schema metadata: {e}")

        if np.issubdtype(np_dtype, np.integer):
            info = np.iinfo(np_dtype)
            low = int(info.min)
            high = int(info.max) + 1
            arr = np.random.randint(low, high, size=shape, dtype=np_dtype)
        elif normalization == "imagenet":
            arr = np.random.randn(*shape).astype(np_dtype)
        else:
            arr = np.random.rand(*shape).astype(np_dtype)

        return arr.tolist()

    def validate(self, data: dict[str, object]) -> None:
        self.schema(**data)
        schema_type = self.input_schema.get("type", "tensor")
        if schema_type == "pow_batch":
            return
        input_data = data.get("input_data", [])
        expected_shape = self.input_schema.get("shape", [])
        self._validate_shape(input_data, expected_shape)

    def _validate_shape(self, data: Any, expected_shape: list[int]) -> None:
        if not expected_shape:
            return

        if not isinstance(data, list):
            raise ValueError(f"Expected list, got {type(data)}")

        if len(data) != expected_shape[0]:
            raise ValueError(
                f"Dimension mismatch: expected {expected_shape[0]}, got {len(data)}"
            )

        if len(expected_shape) == 1:
            for i, item in enumerate(data):
                if isinstance(item, (list, tuple)):
                    raise ValueError(
                        f"Dimension mismatch: expected scalar at index {i}, got nested {type(item).__name__}"
                    )
        elif len(expected_shape) > 1:
            for item in data:
                self._validate_shape(item, expected_shape[1:])

    def process(self, data: dict[str, object]) -> dict[str, object]:
        schema_type = self.input_schema.get("type", "tensor")
        if schema_type == "pow_batch":
            return self._process_pow_batch(data)
        return data

    def _process_pow_batch(self, data: dict[str, object]) -> dict[str, object]:
        scaling = self.input_schema.get("scaling", 100000000)
        c = self._get_pow_constants()

        data["maximum_score"] = [int(v * scaling) for v in data["maximum_score"]]
        data["previous_score"] = [int(v * scaling) for v in data["previous_score"]]
        data["proof_size"] = [int(v * scaling) for v in data["proof_size"]]
        data["minimum_response_time"] = [
            int(v * scaling) for v in data["minimum_response_time"]
        ]
        data["maximum_response_time"] = [
            int(v * scaling) for v in data["maximum_response_time"]
        ]
        data["response_time"] = [int(v * scaling) for v in data["response_time"]]
        data["competition"] = [int(v * scaling) for v in data["competition"]]

        batch_size = self.input_schema.get("batch_size", 256)
        for i in range(16):
            if batch_size - 16 + i < len(data["validator_uid"]):
                data["validator_uid"][batch_size - 16 + i] = secrets.randbits(16)

        constant_keys = {
            "RATE_OF_DECAY": "rate_of_decay",
            "RATE_OF_RECOVERY": "rate_of_recovery",
            "FLATTENING_COEFFICIENT": "flattening_coefficient",
            "PROOF_SIZE_WEIGHT": "proof_size_weight",
            "PROOF_SIZE_THRESHOLD": "proof_size_threshold",
            "COMPETITION_WEIGHT": "competition_weight",
            "RESPONSE_TIME_WEIGHT": "response_time_weight",
            "MAXIMUM_RESPONSE_TIME_DECIMAL": "maximum_response_time_decimal",
        }
        for data_key, const_key in constant_keys.items():
            if data_key not in data:
                data[data_key] = int(c[const_key] * scaling)
        if "scaling" not in data:
            data["scaling"] = scaling

        return data

    def to_array(self) -> list:
        return self.data["input_data"]


class GenericInput(BaseInput):
    schema = BaseModel

    def __init__(
        self, request_type: RequestType, data: dict[str, object] | None = None
    ):
        super().__init__(request_type, data)

    @staticmethod
    def generate() -> dict[str, object]:
        raise NotImplementedError("Generic input does not support generation")

    @staticmethod
    def validate(data: dict[str, object]) -> None:
        pass

    @staticmethod
    def process(data: dict[str, object]) -> dict[str, object]:
        return data
