"""Custom exceptions for validator core operations."""


class ProofException(Exception):
    """Base exception for proof-related errors."""

    def __init__(self, uid: int, circuit: str, message: str = ""):
        self.uid = uid
        self.circuit = circuit
        self.message = message
        super().__init__(self.message)


class EmptyProofException(ProofException):
    """Raised when miner fails to provide a proof."""

    def __init__(self, uid: int, circuit: str, raw_response: str | None = None):
        self.raw_response = raw_response
        message = f"Miner at UID {uid} failed to provide a valid proof for {circuit}."
        super().__init__(uid, circuit, message)


class IncorrectProofException(ProofException):
    """Raised when proof verification fails."""

    def __init__(self, uid: int, circuit: str):
        message = f"Miner at UID {uid} provided an incorrect proof for {circuit}."
        super().__init__(uid, circuit, message)
