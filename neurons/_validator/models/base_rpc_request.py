from pydantic import BaseModel
from execution_layer.circuit import Circuit
from _validator.utils.api import hash_inputs


class QueuedRequestDataModel(BaseModel):
    """
    Base model for requests that are stacked in the validator's queue and waiting to be sent to miners.
    At the moment, that's a Real World Request (RWR) or a Request with one slice of a DSperse model (DSlice).
    """

    circuit: Circuit
    inputs: dict

    model_config = {"arbitrary_types_allowed": True}

    @property
    def hash(self) -> str:
        return hash_inputs(self.inputs)
