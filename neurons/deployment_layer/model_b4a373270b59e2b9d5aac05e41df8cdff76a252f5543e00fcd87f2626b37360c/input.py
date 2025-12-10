from __future__ import annotations

import random

from execution_layer.base_input import BaseInput
from execution_layer.input_registry import InputRegistry
from pydantic import BaseModel

from _validator.models.request_type import RequestType

INPUT_LENGTH = 3072


class CircuitInputSchema(BaseModel):
    input_data: list[list[float]]


@InputRegistry.register(
    "b4a373270b59e2b9d5aac05e41df8cdff76a252f5543e00fcd87f2626b37360c"
)
class CircuitInput(BaseInput):

    schema = CircuitInputSchema

    def __init__(
        self, request_type: RequestType, data: dict[str, object] | None = None
    ):
        super().__init__(request_type, data)

    @staticmethod
    def generate() -> dict[str, object]:
        return {
            "input_data": [[random.uniform(-1.0, 1.0) for _ in range(INPUT_LENGTH)]]
        }

    def validate(self, data: dict[str, object]) -> None:
        return CircuitInputSchema(**data)

    @staticmethod
    def process(data: dict[str, object]) -> dict[str, object]:
        """
        No processing needs to take place, as all inputs are randomized.
        """
        return data
