from __future__ import annotations

import re
from importlib import import_module
from typing import TYPE_CHECKING

from .base_input import BaseInput

if TYPE_CHECKING:
    from execution_layer.circuit import CircuitMetadata

CIRCUIT_ID_PATTERN = re.compile(r"^[a-f0-9]{64}$")


def _validate_circuit_id(circuit_id: str) -> None:
    if not CIRCUIT_ID_PATTERN.match(circuit_id):
        raise ValueError(f"Invalid circuit_id format: {circuit_id!r}")


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
        if circuit_id not in cls._handlers:
            target_module = f"deployment_layer.model_{circuit_id}.input"
            try:
                import_module(target_module)
                if circuit_id not in cls._handlers:
                    raise ValueError(
                        f"Input handler for circuit {circuit_id} was not registered"
                    )
            except ModuleNotFoundError as e:
                if e.name != target_module and not e.name.startswith(
                    target_module + "."
                ):
                    raise
                if (
                    metadata
                    and hasattr(metadata, "input_schema")
                    and metadata.input_schema
                ):
                    return cls._create_generic_handler(metadata.input_schema)
                raise ValueError(f"No input handler found for circuit {circuit_id}")

        return cls._handlers[circuit_id]

    @classmethod
    def _create_generic_handler(cls, input_schema: dict) -> type[BaseInput]:
        from execution_layer.generic_input import GenericInputHandler

        class ConfiguredGenericHandler(GenericInputHandler):
            def __init__(self, request_type, data=None):
                super().__init__(request_type, data, input_schema=input_schema)

        return ConfiguredGenericHandler

    @classmethod
    def has_handler(cls, circuit_id: str) -> bool:
        _validate_circuit_id(circuit_id)
        if circuit_id in cls._handlers:
            return True
        target_module = f"deployment_layer.model_{circuit_id}.input"
        try:
            import_module(target_module)
            return circuit_id in cls._handlers
        except ModuleNotFoundError as e:
            if e.name != target_module and not e.name.startswith(target_module + "."):
                raise
            return False
