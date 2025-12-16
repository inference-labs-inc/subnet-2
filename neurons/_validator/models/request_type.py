from enum import Enum


class RequestType(Enum):
    """
    Enumeration of different types of requests that the validator can send to miner.
     - BENCHMARK: Requests with generated input data for benchmarking purposes.
       In case of empty RWR queue the validator generates some input data to keep miners busy.
     - RWR: Real World Requests with actual input data for real-world inference.
       Validator collects such requests from external users and stacks them in a queue to be sent to miners.
     - DSLICE: That's a tricky one. We use DSperse app for slicing large models into smaller parts.
       And some requests involve sliced model and each slice is sent as a separate request to the miner.
       That each slice request is of type DSLICE.
       At the moment we just stack DSlices to the same RWR queue, and behave as normal RWR requests.
       That means DSLICE request is a part of RWR or BENCHMARK request.
       XXX: Yeah, not very elegant, but we'll improve it later, I promise.
    """

    BENCHMARK = "benchmark_request"
    RWR = "real_world_request"
    DSLICE = "dslice_request"

    def __str__(self) -> str:
        if self == RequestType.BENCHMARK:
            return "Benchmark"
        elif self == RequestType.RWR:
            return "Real World Request"
        elif self == RequestType.DSLICE:
            return "DSperse Request (one slice)"
        else:
            raise ValueError(f"Unknown request type: {self}")


class ValidatorMessage(Enum):
    WINDDOWN = "winddown"
    WINDDOWN_COMPLETE = "winddown_complete"
    COMPETITION_COMPLETE = "competition_complete"

    def __str__(self) -> str:
        return self.value
