from __future__ import annotations
from _validator.models.request_type import RequestType
from pydantic import BaseModel


class BaseInput:
    schema: type[BaseModel] = BaseModel

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

    def generate(self) -> dict[str, object]:
        raise NotImplementedError("Subclass must implement generate()")

    def validate(self, data: dict[str, object]) -> None:
        pass

    def process(self, data: dict[str, object]) -> dict[str, object]:
        return data

    def to_array(self) -> list:
        return list(self.data.values())

    def to_json(self) -> dict[str, object]:
        return self.data
