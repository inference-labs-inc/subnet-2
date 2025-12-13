from dataclasses import dataclass
from typing import Any

from _validator.models.base_rpc_request import QueuedRequestDataModel
from _validator.models.request_type import RequestType
from execution_layer.circuit import Circuit
from execution_layer.generic_input import GenericInput


@dataclass
class Request:
    """
    A request to be sent to a miner.
    """

    uid: int
    ip: str
    port: int
    hotkey: str
    coldkey: str
    url_path: str
    request_type: RequestType
    circuit: Circuit | None = None
    data: dict[str, Any] | None = None
    inputs: GenericInput | None = None
    dsperse_slice_num: int | None = None
    dsperse_run_uid: str | None = None
    # next one is used only for rescheduling DSlice and RWR requests in case of failure
    queued_request: QueuedRequestDataModel | None = None
    # `external_request_hash` is the hash of the original request from external API user
    # we use it to report back results to `ValidatorAPI`` class. It sends the results to the user.
    external_request_hash: str | None = None
    # `guard_hash` is the hash used by HashGuard to prevent duplicate requests
    # It's calculated from the inputs of the request.
    guard_hash: str | None = None
    response_time: float | None = None
    save: bool = False
