import hashlib
from execution_layer.base_input import BaseInput


def hash_inputs(inputs: BaseInput | dict) -> str:
    """
    Hashes inputs to proof of weights, excluding dynamic fields.

    Args:
        inputs (dict): The inputs to hash.

    Returns:
        str: The hashed inputs.
    """
    if isinstance(inputs, BaseInput):
        inputs = inputs.to_json()
    filtered_inputs = {
        k: v
        for k, v in inputs.items()
        if k not in ["validator_uid", "nonce", "uid_responsible_for_proof"]
    }
    return hashlib.sha256(str(filtered_inputs).encode()).hexdigest()
