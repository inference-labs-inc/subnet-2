from dataclasses import dataclass, field


@dataclass
class NeuronState:
    hotkey: str
    uid: int
    sota_relative_score: float
    proof_size: float
    response_time: float
    verification_result: bool
    raw_accuracy: float
    hash: str
    rank_overall: int = 999
    rank_accuracy: int = 999
    rank_proof_size: int = 999
    rank_response_time: int = 999
    historical_best_sota_score: int = 0
    historical_improvement_rate: float = 0.0
    verification_rate: float = 1.0
    relative_to_sota: dict[str, float] = field(default_factory=dict)
