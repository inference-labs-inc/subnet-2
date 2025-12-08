from pydantic import BaseModel


class CircuitFiles(BaseModel):
    verification_key: str
    proving_key: str
    settings: str
    circuit: str
    hash: str
