from __future__ import annotations
from pydantic import BaseModel
from execution_layer.base_input import BaseInput
from execution_layer.input_registry import InputRegistry
from _validator.models.request_type import RequestType
import random
import secrets

TOP_TIER_PCT = 0.1
NEXT_TIER_PCT = 0.4
TOP_TIER_WEIGHT = 0.7
NEXT_TIER_WEIGHT = 0.2
BOTTOM_TIER_WEIGHT = 0.1
BATCH_SIZE = 256


class CircuitInputSchema(BaseModel):
    scores: list[float]
    top_tier_pct: list[float]
    next_tier_pct: list[float]
    top_tier_weight: list[float]
    next_tier_weight: list[float]
    bottom_tier_weight: list[float]
    nonce: list[float]


@InputRegistry.register(
    "43ecaacaded5ed16c9e08bc054366e409c7925245eca547472b27f2a61469cc5"
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
            "scores": [random.random() for _ in range(BATCH_SIZE)],
            "top_tier_pct": [TOP_TIER_PCT],
            "next_tier_pct": [NEXT_TIER_PCT],
            "top_tier_weight": [TOP_TIER_WEIGHT],
            "next_tier_weight": [NEXT_TIER_WEIGHT],
            "bottom_tier_weight": [BOTTOM_TIER_WEIGHT],
            "nonce": [secrets.randbits(32)],
        }

    @staticmethod
    def validate(data: dict[str, object]) -> None:
        return CircuitInputSchema(**data)

    @staticmethod
    def process(data: dict[str, object]) -> dict[str, object]:
        """
        Add a random nonce to ensure that the request is not reused.
        """
        data["nonce"] = [secrets.randbits(32)]
        return data
