from collections import OrderedDict
import json
import hashlib

from execution_layer.base_input import BaseInput


class HashGuard:
    """
    A safety checker to ensure input data is never repeated.
    Uses SHA-256 for consistent hashing across sessions and sorted keys for deterministic JSON.
    Uses an OrderedDict for O(1) lookup, insertion-order eviction, and O(1) removal.
    """

    MAX_HASHES = 32768

    def __init__(self):
        self._hashes: OrderedDict[str, None] = OrderedDict()

    def remove_hash(self, hash_value: str) -> None:
        if hash_value:
            self._hashes.pop(hash_value, None)

    def check_hash(self, input: BaseInput) -> str:
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

        if hash_value in self._hashes:
            raise ValueError("Hash already exists")

        if len(self._hashes) >= self.MAX_HASHES:
            self._hashes.popitem(last=False)

        self._hashes[hash_value] = None
        return hash_value
