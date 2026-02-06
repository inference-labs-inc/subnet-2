from typing import Optional

from pydantic import Field

from _validator.models.base_rpc_request import QueuedRequestDataModel
from _validator.models.request_type import RequestType
from execution_layer.circuit import ProofSystem


class DSliceQueuedProofRequest(QueuedRequestDataModel):
    """
    Request for a DSperse slice.

    In standard mode, outputs are provided by the validator.
    In incremental mode (compute_outputs=True), outputs is None and the miner
    computes them during proof generation.
    """

    request_type: RequestType = RequestType.DSLICE
    proof_system: ProofSystem = Field(..., description="Proof system for the slice")
    slice_num: str = Field(..., description="Num of the DSperse slice")
    run_uid: str = Field(..., description="UID of the DSperse run")
    outputs: Optional[dict] = Field(
        None, description="Expected outputs (None for incremental mode)"
    )
    compute_outputs: bool = Field(
        False, description="If True, miner computes and returns outputs"
    )
    is_tile: bool = Field(False, description="Whether this is a tile request")
    tile_idx: Optional[int] = Field(None, description="Tile index for tiled slices")
    task_id: Optional[str] = Field(None, description="Tile task ID for result tracking")
