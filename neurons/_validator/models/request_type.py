from enum import Enum


class RequestType(Enum):
    BENCHMARK = "benchmark_request"
    RWR = "real_world_request"

    def __str__(self) -> str:
        if self == RequestType.BENCHMARK:
            return "Benchmark"
        elif self == RequestType.RWR:
            return "Real World Request"
        else:
            raise ValueError(f"Unknown request type: {self}")


class ValidatorMessage(Enum):
    WINDDOWN = "winddown"
    WINDDOWN_COMPLETE = "winddown_complete"
    COMPETITION_COMPLETE = "competition_complete"

    def __str__(self) -> str:
        return self.value
