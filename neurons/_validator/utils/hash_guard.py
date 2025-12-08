from execution_layer.base_input import BaseInput
import bittensor as bt
import json
import hashlib
from collections import deque


class HashGuard:
    """
    A safety checker to ensure input data is never repeated.
    Uses SHA-256 for consistent hashing across sessions and sorted keys for deterministic JSON.
    Uses a set for O(1) lookups and a deque for FIFO order.
    """

    MAX_HASHES = 32768

    def __init__(self):
        self.hash_set = set()
        self.hash_queue = deque(maxlen=self.MAX_HASHES)

    def check_hash(self, input: BaseInput) -> None:

        if isinstance(input, BaseInput):
            input = input.to_json()

        def sort_dict(d):
            if isinstance(d, dict):
                return {k: sort_dict(v) for k, v in sorted(d.items())}
            if isinstance(d, list):
                return [sort_dict(x) for x in d]
            return d

        sorted_input = sort_dict(input)
        json_str = json.dumps(sorted_input, sort_keys=True)
        hash_value = hashlib.sha256(json_str.encode()).hexdigest()

        if hash_value in self.hash_set:
            bt.logging.error(f"Hash already exists: {hash_value}. Inputs: {input}")
            raise ValueError("Hash already exists")

        if len(self.hash_queue) == self.MAX_HASHES:
            old_hash = self.hash_queue.popleft()
            self.hash_set.remove(old_hash)

        self.hash_set.add(hash_value)
        self.hash_queue.append(hash_value)
