"""
This module contains the reward function for the validator.
"""

import torch
from torch import nn

RATE_OF_DECAY = 0.4
RATE_OF_RECOVERY = 0.1
FLATTENING_COEFFICIENT = 0.9
PROOF_SIZE_THRESHOLD = 3648
PROOF_SIZE_WEIGHT = 0
RESPONSE_TIME_WEIGHT = 1
MAXIMUM_RESPONSE_TIME_DECIMAL = 0.99


class Reward(nn.Module):
    """
    This module is responsible for calculating the reward for a miner based on the provided score, verification_result,
    response_time, and proof_size in it's forward pass.
    """

    def __init__(self):
        super().__init__()
        self.RATE_OF_DECAY = torch.tensor(RATE_OF_DECAY)
        self.RATE_OF_RECOVERY = torch.tensor(RATE_OF_RECOVERY)
        self.FLATTENING_COEFFICIENT = torch.tensor(FLATTENING_COEFFICIENT)
        self.PROOF_SIZE_THRESHOLD = torch.tensor(PROOF_SIZE_THRESHOLD)
        self.PROOF_SIZE_WEIGHT = torch.tensor(PROOF_SIZE_WEIGHT)
        self.RESPONSE_TIME_WEIGHT = torch.tensor(RESPONSE_TIME_WEIGHT)
        self.MAXIMUM_RESPONSE_TIME_DECIMAL = torch.tensor(MAXIMUM_RESPONSE_TIME_DECIMAL)

    def shifted_tan(self, x: torch.FloatTensor) -> torch.FloatTensor:
        """
        Shifted tangent curve
        """
        return torch.tan(
            torch.mul(
                torch.mul(torch.sub(x, torch.tensor(0.5)), torch.pi),
                self.FLATTENING_COEFFICIENT,
            )
        )

    def tan_shift_difference(self, x: torch.FloatTensor) -> torch.FloatTensor:
        """
        Difference
        """
        return torch.sub(self.shifted_tan(x), self.shifted_tan(torch.tensor(0.0)))

    def normalized_tangent_curve(self, x: torch.FloatTensor) -> torch.FloatTensor:
        return torch.div(
            self.tan_shift_difference(x), self.tan_shift_difference(torch.tensor(1.0))
        )

    def forward(
        self,
        maximum_score: torch.FloatTensor,
        previous_score: torch.FloatTensor,
        verified: torch.BoolTensor,
        proof_size: torch.IntTensor,
        response_time: torch.FloatTensor,
        maximum_response_time: torch.FloatTensor,
        minimum_response_time: torch.FloatTensor,
        block_number: torch.IntTensor,
        validator_uid: torch.IntTensor,
        miner_uid: torch.IntTensor,
    ):
        """
        This method calculates the reward for a miner based on the provided score, verification_result,
        response_time, and proof_size using a neural network module.
        Positional Arguments:
            max_score (FloatTensor): The maximum score for the miner.
            score (FloatTensor): The current score for the miner.
            verified (BoolTensor): Whether the response that the miner submitted was valid.
            proof_size (FloatTensor): The size of the proof.
            response_time (FloatTensor): The time taken to respond to the query.
            maximum_response_time (FloatTensor): The maximum response time received from validator queries
            minimum_response_time (FloatTensor): The minimum response time received from validator queries
            validator_hotkey (FloatTensor[]): Ascii representation of the validator's hotkey
            block_number (FloatTensor): The block number of the block that the response was submitted in.
        Returns:
            [new_score, validator_hotkey, block_number]
        """
        # Determine rate of scoring change based on whether the response was verified
        rate_of_change = torch.where(
            verified, self.RATE_OF_RECOVERY, self.RATE_OF_DECAY
        )

        # Ensure maximum_response_time is not equal to minimum_response_time to avoid division by zero
        # Adding a small constant like 1 ensures the divisor is non-zero.
        adjusted_max_response_time = torch.where(
            torch.eq(maximum_response_time, minimum_response_time),
            maximum_response_time + 1,
            maximum_response_time,
        )

        # Normalize the response time into a decimal between zero and the maximum response time decimal
        # Maximum is capped at maximum response time decimal here to limit degree of score reduction
        # in cases of very poor performance
        response_time_normalized = torch.clamp(
            torch.div(
                torch.sub(response_time, minimum_response_time),
                torch.sub(adjusted_max_response_time, minimum_response_time),
            ),
            0,
            self.MAXIMUM_RESPONSE_TIME_DECIMAL,
        )

        # Calculate reward metrics from both response time and proof size
        response_time_reward_metric = torch.mul(
            self.RESPONSE_TIME_WEIGHT,
            torch.sub(
                torch.tensor(1), self.normalized_tangent_curve(response_time_normalized)
            ),
        )
        proof_size_reward_metric = torch.mul(
            self.PROOF_SIZE_WEIGHT,
            torch.clamp(
                proof_size / self.PROOF_SIZE_THRESHOLD, torch.tensor(0), torch.tensor(1)
            ),
        )

        # Combine reward metrics to provide a final score based on provided inputs
        calculated_score_fraction = torch.clamp(
            torch.sub(response_time_reward_metric, proof_size_reward_metric),
            torch.tensor(0),
            torch.tensor(1),
        )

        # Adjust the maximum score for the miner based on calculated metrics
        maximum_score = torch.mul(maximum_score, calculated_score_fraction)

        # Get the distance of the previous score from the new maximum or zero, depending on verification status
        distance_from_score = torch.where(
            verified, torch.sub(maximum_score, previous_score), previous_score
        )

        # Calculate the difference in scoring that will be applied based on the rate and distance from target score
        change_in_score = torch.mul(rate_of_change, distance_from_score)

        # Provide a new score based on their previous score and change in score. In cases where verified is false,
        # scores are always decreased.
        new_score = torch.where(
            verified,
            previous_score + change_in_score,
            previous_score - change_in_score,
        )

        # Technically, new score is the only output that matters since we verify all inputs.
        # These metadata fields are included to force torch.jit to leave them in when converting to ONNX
        return [new_score, block_number, miner_uid, validator_uid]
