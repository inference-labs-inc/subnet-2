from __future__ import annotations
from pydantic import BaseModel
from execution_layer.base_input import BaseInput
from execution_layer.input_registry import InputRegistry
from _validator.models.request_type import RequestType
import random
import secrets

SUCCESS_WEIGHT = 1
DIFFICULTY_WEIGHT = 1
TIME_ELAPSED_WEIGHT = 0.3
FAILED_PENALTY_WEIGHT = 0.4
ALLOCATION_WEIGHT = 0.21
POW_MIN_DIFFICULTY = 7
POW_MAX_DIFFICULTY = 12
POW_TIMEOUT = 30.0
BATCH_SIZE = 256


class CircuitInputSchema(BaseModel):
    challenge_attempts: list[int]
    challenge_successes: list[int]
    last_20_challenge_failed: list[int]
    challenge_elapsed_time_avg: list[float]
    last_20_difficulty_avg: list[float]
    has_docker: list[bool]
    uid: list[int]
    allocated_uids: list[int]
    penalized_uids: list[int]
    validator_uids: list[int]
    success_weight: list[float]
    difficulty_weight: list[float]
    time_elapsed_weight: list[float]
    failed_penalty_weight: list[float]
    allocation_weight: list[float]
    pow_timeout: list[float]
    pow_min_difficulty: list[float]
    pow_max_difficulty: list[float]
    nonce: list[int]


@InputRegistry.register(
    "1876cfa9fb3c418b2559f3f7074db20565b5ca7237efdd43b907d9d697a452c4"
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
            "challenge_attempts": [random.randint(5, 10) for _ in range(BATCH_SIZE)],
            "challenge_successes": [random.randint(4, 8) for _ in range(BATCH_SIZE)],
            "last_20_challenge_failed": [
                random.randint(0, 20) for _ in range(BATCH_SIZE)
            ],
            "challenge_elapsed_time_avg": [
                4.0 + random.random() * 4.0 for _ in range(BATCH_SIZE)
            ],
            "last_20_difficulty_avg": [
                POW_MIN_DIFFICULTY
                + random.random() * (POW_MAX_DIFFICULTY - POW_MIN_DIFFICULTY)
                for _ in range(BATCH_SIZE)
            ],
            "has_docker": [random.random() < 0.5 for _ in range(BATCH_SIZE)],
            "uid": [random.randint(0, 255) for _ in range(BATCH_SIZE)],
            "allocated_uids": [random.randint(0, 255) for _ in range(256)],
            "penalized_uids": [random.randint(0, 255) for _ in range(256)],
            "validator_uids": [random.randint(0, 255) for _ in range(256)],
            "success_weight": [SUCCESS_WEIGHT],
            "difficulty_weight": [DIFFICULTY_WEIGHT],
            "time_elapsed_weight": [TIME_ELAPSED_WEIGHT],
            "failed_penalty_weight": [FAILED_PENALTY_WEIGHT],
            "allocation_weight": [ALLOCATION_WEIGHT],
            "pow_timeout": [POW_TIMEOUT],
            "pow_min_difficulty": [POW_MIN_DIFFICULTY],
            "pow_max_difficulty": [POW_MAX_DIFFICULTY],
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
