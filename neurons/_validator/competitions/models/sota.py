from dataclasses import dataclass


@dataclass
class SotaState:
    sota_relative_score: float = 0.0
    hash: str | None = None
    hotkey: str | None = None
    uid: int | None = None
    proof_size: float = float("inf")
    response_time: float = float("inf")
    timestamp: int = 0
    raw_accuracy: float = 0.0
