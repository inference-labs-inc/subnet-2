from _validator.models.base_rpc_request import QueuedRequestDataModel
from pydantic import Field


class DSliceQueuedProofRequest(QueuedRequestDataModel):
    """
    Request for a DSperse slice.
    """

    slice_num: str = Field(..., description="Num of the DSperse slice")
    run_uid: str = Field(..., description="UID of the DSperse run")
    outputs: dict = Field(..., description="Outputs of the DSperse slice")
