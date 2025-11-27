from __future__ import annotations
from pydantic import BaseModel
from execution_layer.base_input import BaseInput
from execution_layer.input_registry import InputRegistry
from _validator.models.request_type import RequestType
import random


class NetInputSchema(BaseModel):
    # DSperse runner expects an input.json shaped like {"input_data": ...}
    # Keep this as a list of lists of floats to be compatible with generic handling.
    input_data: list[list[float]]


@InputRegistry.register("model_net")
class NetInput(BaseInput):
    """
    Input generator/validator for the model_net DSperse deployment.
    Produces a simple vector input wrapped under key "input_data".
    """

    schema = NetInputSchema

    def __init__(self, request_type: RequestType, data: dict[str, object] | None = None):
        super().__init__(request_type, data)

    @staticmethod
    def generate() -> dict[str, object]:
        # Generate a simple 1x16 vector of floats in [0,1). Adjust the length if your model expects a different size.
        length = 16
        return {
            "input_data": [[random.random() for _ in range(length)]],
        }

    @staticmethod
    def validate(data: dict[str, object]) -> None:
        return NetInputSchema(**data)

    @staticmethod
    def process(data: dict[str, object]) -> dict[str, object]:
        # No additional processing required; passthrough.
        return data
