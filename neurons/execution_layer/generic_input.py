from __future__ import annotations
from execution_layer.base_input import BaseInput
from _validator.models.request_type import RequestType
from pydantic import BaseModel


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
