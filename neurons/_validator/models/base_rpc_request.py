from pydantic import BaseModel
from execution_layer.circuit import Circuit
from _validator.utils.api import hash_inputs


class RealWorldRequest(BaseModel):
    circuit: Circuit
    inputs: dict

    model_config = {"arbitrary_types_allowed": True}

    @property
    def hash(self) -> str:
        return hash_inputs(self.inputs)
