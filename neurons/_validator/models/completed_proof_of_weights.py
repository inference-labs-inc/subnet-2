from __future__ import annotations
from dataclasses import dataclass

from attrs import field


@dataclass
class CompletedProofOfWeightsItem:
    """
    A completed proof of weights item, to be logged to the chain.
    """

    signals: list[str] | None = field(default=None)
    proof: dict | str | None = field(default=None)
    model_id: str | None = field(default=None)
    netuid: int | None = field(default=None)

    def to_remark(self) -> dict:
        return {
            "type": "proof_of_weights",
            "signals": self.signals,
            "proof": self.proof,
            "verification_key": self.model_id,
            "netuid": self.netuid,
        }
