from pydantic import Field

from _validator.models.base_rpc_request import QueuedRequestDataModel
from _validator.models.request_type import RequestType
from execution_layer.circuit import ProofSystem


class DSliceQueuedProofRequest(QueuedRequestDataModel):
    """
    Request for a DSperse slice.
    """

    request_type: RequestType = RequestType.DSLICE
    proof_system: ProofSystem = ProofSystem.JSTPROOF
    slice_num: str = Field(..., description="Num of the DSperse slice")
    run_uid: str = Field(..., description="UID of the DSperse run")
    outputs: dict = Field(..., description="Outputs of the DSperse slice")
