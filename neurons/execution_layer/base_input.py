from __future__ import annotations
from abc import ABC, abstractmethod
from _validator.models.request_type import RequestType
from pydantic import BaseModel


class BaseInput(ABC):
    """
    Base class for circuit-specific input data. Stores and provides interface
    for manipulating circuit input data.
    """

    schema: type[BaseModel]

    def __init__(
        self,
        request_type: RequestType,
        data: dict[str, object] | None = None,
    ):
        self.request_type = request_type
        if request_type == RequestType.BENCHMARK:
            self.data = self.generate()
        else:
            if data is None:
                raise ValueError("Data must be provided for non-benchmark requests")
            self.validate(data)
            self.data = self.process(data)

    @staticmethod
    @abstractmethod
    def generate() -> dict[str, object]:
        """Generates new benchmarking input data for this circuit"""
        pass

    @staticmethod
    @abstractmethod
    def validate(self, data: dict[str, object]) -> None:
        """Validates input data against circuit-specific schema. Raises ValueError if invalid."""
        pass

    @staticmethod
    @abstractmethod
    def process(self, data: dict[str, object]) -> dict[str, object]:
        """Processes raw input data into standardized format"""
        pass

    def to_array(self) -> list:
        """Converts the data to array format"""
        return list(self.data.values())

    def to_json(self) -> dict[str, object]:
        """Returns the data in JSON-compatible format"""
        return self.data
