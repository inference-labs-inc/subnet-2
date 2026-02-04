from __future__ import annotations
from dataclasses import dataclass


@dataclass
class CompletedProofOfWeightsItem:
    """
    A completed proof of weights item, to be logged to the chain.
    """

    signals: list[str] | None = None
    proof: dict | str | None = None
    model_id: str | None = None
    netuid: int | None = None

    def to_remark(self) -> dict:
        return {
            "type": "proof_of_weights",
            "signals": self.signals,
            "proof": self.proof,
            "verification_key": self.model_id,
            "netuid": self.netuid,
        }
