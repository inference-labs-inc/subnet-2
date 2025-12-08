from __future__ import annotations
from importlib import import_module
from .base_input import BaseInput


class InputRegistry:
    """Registry for circuit-specific input handlers"""

    _handlers: dict[str, type[BaseInput]] = {}

    @classmethod
    def register(cls, circuit_id: str):
        """Registers a circuit input handler class for the given circuit ID"""

        def decorator(handler_class: type[BaseInput]):
            cls._handlers[circuit_id] = handler_class
            return handler_class

        return decorator

    @classmethod
    def get_handler(cls, circuit_id: str) -> type[BaseInput]:
        """
        Gets the registered input handler for a circuit ID.
        Attempts to import the handler module if not already registered.

        Args:
            circuit_id: The ID of the circuit to get the handler for

        Returns:
            The input handler class for the circuit

        Raises:
            ValueError: If no handler is found or registration fails
        """
        if circuit_id not in cls._handlers:
            try:
                import_module(f"deployment_layer.model_{circuit_id}.input")
                if circuit_id not in cls._handlers:
                    raise ValueError(
                        f"Input handler for circuit {circuit_id} was not registered"
                    )
            except ImportError as e:
                raise ValueError(
                    f"No input handler found for circuit {circuit_id}: {e}"
                )

        return cls._handlers[circuit_id]
