import json
import os
import traceback
from dataclasses import dataclass
import bittensor as bt
import torch
import time
from typing import Optional
from _validator.utils.pps import ProofPublishingService

from substrateinterface import ExtrinsicReceipt, Keypair
from _validator.models.miner_response import (
    MinerResponse,
)
from constants import (
    DEFAULT_MAX_SCORE,
    DEFAULT_PROOF_SIZE,
    VALIDATOR_REQUEST_TIMEOUT_SECONDS,
    PPS_URL,
    TESTNET_PPS_URL,
)

# Constants

POW_DIRECTORY = os.path.join(
    os.path.dirname(os.path.realpath(__file__)), "..", "proof_of_weights"
)
if not os.path.exists(POW_DIRECTORY):
    os.makedirs(POW_DIRECTORY)


POW_RECEIPT_DIRECTORY = os.path.join(POW_DIRECTORY, "receipts")
if not os.path.exists(POW_RECEIPT_DIRECTORY):
    os.makedirs(POW_RECEIPT_DIRECTORY)


def to_tensor(value, dtype):
    if isinstance(value, torch.Tensor):
        return value.clone().detach().to(dtype)
    return torch.tensor(value, dtype=dtype)


dummy_miner_response = MinerResponse.empty()


@dataclass
class ProofOfWeightsItem:
    maximum_score: torch.Tensor
    previous_score: torch.Tensor
    verified: torch.Tensor
    proof_size: torch.Tensor
    response_time: torch.Tensor
    competition: torch.Tensor
    maximum_response_time: torch.Tensor
    minimum_response_time: torch.Tensor
    block_number: torch.Tensor
    validator_uid: torch.Tensor
    miner_uid: torch.Tensor

    def __post_init__(self):
        self.maximum_score = to_tensor(self.maximum_score, torch.float32)
        self.previous_score = to_tensor(self.previous_score, torch.float32)
        self.verified = to_tensor(self.verified, torch.bool)
        self.proof_size = to_tensor(self.proof_size, torch.int64)
        self.response_time = to_tensor(self.response_time, torch.float32)
        self.maximum_response_time = to_tensor(
            self.maximum_response_time, torch.float32
        )
        self.competition = to_tensor(self.competition, torch.float32)
        self.minimum_response_time = to_tensor(
            self.minimum_response_time, torch.float32
        )
        self.block_number = to_tensor(self.block_number, torch.int64)
        self.validator_uid = to_tensor(self.validator_uid, torch.int64)
        self.miner_uid = to_tensor(self.miner_uid, torch.int64)

    @staticmethod
    def for_competition(
        uid: int,
        maximum_score: float,
        competition_score: float,
        block_number: int,
        validator_uid: int,
    ):
        return ProofOfWeightsItem(
            maximum_score=maximum_score,
            previous_score=0,
            verified=torch.tensor(True),
            proof_size=torch.tensor(1),
            response_time=torch.tensor(1),
            competition=torch.tensor(competition_score),
            maximum_response_time=torch.tensor(1),
            minimum_response_time=torch.tensor(0),
            block_number=torch.tensor(block_number),
            validator_uid=torch.tensor(validator_uid),
            miner_uid=torch.tensor(uid, dtype=torch.int64),
        )

    @staticmethod
    def from_miner_response(
        response: MinerResponse,
        maximum_score,
        previous_score,
        maximum_response_time,
        minimum_response_time,
        block_number,
        validator_uid,
        competition,
    ):
        max_val_float = float(maximum_response_time)
        min_val_float = float(minimum_response_time)

        if (max_val_float - min_val_float) < 1.0:
            adjusted_max_response_time = min_val_float + 1.0
        else:
            adjusted_max_response_time = max_val_float

        return ProofOfWeightsItem(
            maximum_score=maximum_score,
            previous_score=previous_score,
            verified=torch.tensor(response.verification_result, dtype=torch.bool),
            proof_size=torch.tensor(response.proof_size, dtype=torch.int64),
            response_time=(
                torch.tensor(response.response_time, dtype=torch.float32)
                if response.verification_result
                else torch.tensor(
                    adjusted_max_response_time, dtype=torch.float32
                )  # Use adjusted time here as well for consistency if not verified
            ),
            competition=competition,
            maximum_response_time=torch.tensor(
                adjusted_max_response_time, dtype=torch.float32
            ),
            minimum_response_time=torch.tensor(min_val_float, dtype=torch.float32),
            block_number=block_number,
            validator_uid=validator_uid,
            miner_uid=torch.tensor(response.uid, dtype=torch.int64),
        )

    @staticmethod
    def pad_items(
        items: list["ProofOfWeightsItem"], target_item_count: int = 256
    ) -> list["ProofOfWeightsItem"]:
        if len(items) == 0:
            return [ProofOfWeightsItem.empty()] * target_item_count

        # Pad or truncate the input list to exactly target_item_count
        if len(items) < target_item_count:
            items.extend(
                [ProofOfWeightsItem.empty()] * (target_item_count - len(items))
            )
        elif len(items) > target_item_count:
            items = items[-target_item_count:]

        return items

    @staticmethod
    def empty():
        return ProofOfWeightsItem(
            maximum_score=torch.tensor(DEFAULT_MAX_SCORE, dtype=torch.float32),
            previous_score=torch.tensor(0, dtype=torch.float32),
            verified=torch.tensor(0, dtype=torch.int64),
            proof_size=torch.tensor(DEFAULT_PROOF_SIZE, dtype=torch.int64),
            response_time=torch.tensor(
                VALIDATOR_REQUEST_TIMEOUT_SECONDS, dtype=torch.float32
            ),
            competition=torch.tensor(0, dtype=torch.float32),
            maximum_response_time=torch.tensor(
                VALIDATOR_REQUEST_TIMEOUT_SECONDS, dtype=torch.float32
            ),
            minimum_response_time=torch.tensor(0, dtype=torch.float32),
            block_number=torch.tensor(0, dtype=torch.int64),
            validator_uid=torch.tensor(0, dtype=torch.int64),
            miner_uid=torch.tensor(-1, dtype=torch.int64),
        )

    @staticmethod
    def to_dict_list(items: list["ProofOfWeightsItem"]):
        result = {
            "maximum_score": [],
            "previous_score": [],
            "verified": [],
            "proof_size": [],
            "response_time": [],
            "competition": [],
            "maximum_response_time": [],
            "minimum_response_time": [],
            "block_number": [],
            "validator_uid": [],
            "miner_uid": [],
        }
        for item in items:
            result["maximum_score"].append(item.maximum_score.item())
            result["previous_score"].append(item.previous_score.item())
            result["verified"].append(item.verified.item())
            result["proof_size"].append(item.proof_size.item())
            result["response_time"].append(item.response_time.item())
            result["competition"].append(item.competition.item())
            result["maximum_response_time"].append(item.maximum_response_time.item())
            result["minimum_response_time"].append(item.minimum_response_time.item())
            result["block_number"].append(item.block_number.item())
            result["validator_uid"].append(item.validator_uid.item())
            result["miner_uid"].append(item.miner_uid.item())
        return result


def save_proof_of_weights(
    public_signals: list,
    proof: str,
    metadata: dict,
    hotkey: Keypair,
    is_testnet: bool = False,
    proof_filename: Optional[str] = None,
):
    """
    Save the proof of weights to a JSON file.

    Args:
        public_signals (list): The public signals data as a JSON array.
        proof (str): The proof.
        metadata (dict): Additional metadata for the proof.
        hotkey (Keypair): The hotkey used to sign the proof.
        proof_filename (Optional[str]): Custom filename for the proof file.
    """
    try:
        if proof_filename is None:
            proof_filename = str(int(time.time()))

        file_path = os.path.join(POW_DIRECTORY, f"{proof_filename}.json")

        proof_json = {
            "public_signals": public_signals,
            "proof": proof,
            "metadata": metadata,
        }

        pps = ProofPublishingService(PPS_URL if not is_testnet else TESTNET_PPS_URL)
        response = pps.publish_proof(proof_json, hotkey)

        if response is None:
            bt.logging.error("Failed to publish proof of weights, saving to disk.")
            with open(file_path, "w", encoding="utf-8") as f:
                json.dump(proof_json, f)
            return
        else:
            bt.logging.success(f"Proof of weights receipt saved to {file_path}")
            with open(file_path, "w", encoding="utf-8") as f:
                json.dump(response, f)
    except Exception as e:
        bt.logging.error(f"Error saving proof of weights to file: {e}")
        traceback.print_exc()


def save_receipt(receipt: ExtrinsicReceipt):
    with open(
        os.path.join(POW_RECEIPT_DIRECTORY, f"{receipt.extrinsic_hash}.json"),
        "w",
        encoding="utf-8",
    ) as f:
        json.dump({k: str(v) for k, v in receipt.__dict__.items()}, f, indent=4)
