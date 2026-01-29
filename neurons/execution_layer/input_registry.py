from __future__ import annotations

import re
from typing import TYPE_CHECKING

from .base_input import BaseInput

if TYPE_CHECKING:
    from execution_layer.circuit import CircuitMetadata

CIRCUIT_ID_PATTERN = re.compile(r"^[a-f0-9]{64}$")


def _validate_circuit_id(circuit_id: str) -> None:
    if not CIRCUIT_ID_PATTERN.match(circuit_id):
        raise ValueError(f"Invalid circuit_id format: {circuit_id!r}")


class GenericInputFactory:
    def __init__(self, input_schema: dict):
        self.input_schema = input_schema
        from execution_layer.generic_input import create_schema_from_metadata

        self.schema = create_schema_from_metadata(input_schema)

    def __call__(self, request_type, data=None):
        from execution_layer.generic_input import GenericInputHandler

        return GenericInputHandler(request_type, data, input_schema=self.input_schema)

    def __reduce__(self):
        return (GenericInputFactory, (self.input_schema,))


class InputRegistry:
    _handlers: dict[str, type[BaseInput]] = {}

    @classmethod
    def register(cls, circuit_id: str):
        def decorator(handler_class: type[BaseInput]):
            cls._handlers[circuit_id] = handler_class
            return handler_class

        return decorator

    @classmethod
    def get_handler(
        cls, circuit_id: str, metadata: "CircuitMetadata | None" = None
    ) -> type[BaseInput]:
        _validate_circuit_id(circuit_id)
        if circuit_id in cls._handlers:
            return cls._handlers[circuit_id]
        if metadata and hasattr(metadata, "input_schema") and metadata.input_schema:
            return cls._create_generic_handler(metadata.input_schema)
        raise ValueError(f"No input handler found for circuit {circuit_id}")

    @classmethod
    def _create_generic_handler(cls, input_schema: dict) -> GenericInputFactory:
        return GenericInputFactory(input_schema)
