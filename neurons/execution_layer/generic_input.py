from __future__ import annotations

from typing import Any

import numpy as np
from pydantic import BaseModel

from _validator.models.request_type import RequestType
from execution_layer.base_input import BaseInput


class TensorInputSchema(BaseModel):
    input_data: list


def create_schema_from_metadata(input_schema: dict) -> type[BaseModel]:
    schema_type = input_schema.get("type")
    if schema_type == "tensor":
        return TensorInputSchema
    raise ValueError(f"Unsupported input schema type: '{schema_type}'")


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

    def generate(self) -> dict[str, object]:
        shape = self.input_schema.get("shape", [1, 3, 224, 224])
        dtype = self.input_schema.get("dtype", "float32")
        normalization = self.input_schema.get("normalization")

        input_data = self._generate_tensor(shape, dtype, normalization)
        return {"input_data": input_data}

    def _generate_tensor(
        self, shape: list[int], dtype: str, normalization: str | None
    ) -> list:
        try:
            np_dtype = np.dtype(dtype)
        except (TypeError, ValueError) as e:
            raise ValueError(f"Invalid dtype '{dtype}' from input schema metadata: {e}")

        if np.issubdtype(np_dtype, np.integer):
            info = np.iinfo(np_dtype)
            arr = np.random.randint(info.min, info.max + 1, size=shape, dtype=np_dtype)
        elif normalization == "imagenet":
            arr = np.random.randn(*shape).astype(np_dtype)
        else:
            arr = np.random.rand(*shape).astype(np_dtype)

        return arr.tolist()

    def validate(self, data: dict[str, object]) -> None:
        self.schema(**data)
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

    @staticmethod
    def process(data: dict[str, object]) -> dict[str, object]:
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
