import os
import bittensor as bt
from dataclasses import dataclass, field
from utils.system import get_temp_folder


@dataclass
class SessionStorage:
    model_id: str
    session_uuid: str
    base_path: str = field(default_factory=get_temp_folder)
    input_path: str = field(init=False)
    witness_path: str = field(init=False)
    proof_path: str = field(init=False)
    public_path: str = field(init=False)

    def __post_init__(self):
        if not os.path.exists(self.base_path):
            os.makedirs(self.base_path)
        self.input_path = os.path.join(
            self.base_path, f"input_{self.model_id}_{self.session_uuid}.json"
        )
        self.witness_path = os.path.join(
            self.base_path, f"witness_{self.model_id}_{self.session_uuid}.json"
        )
        self.proof_path = os.path.join(
            self.base_path, f"proof_{self.model_id}_{self.session_uuid}.json"
        )
        self.public_path = os.path.join(
            self.base_path, f"proof_{self.model_id}_{self.session_uuid}.public.json"
        )
        bt.logging.debug(
            f"SessionStorage initialized with model_id: {self.model_id} and session_uuid: {self.session_uuid}"
        )
        bt.logging.trace(f"Input path: {self.input_path}")
        bt.logging.trace(f"Witness path: {self.witness_path}")
        bt.logging.trace(f"Proof path: {self.proof_path}")
