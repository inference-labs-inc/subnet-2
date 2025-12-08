import os
import json
import bittensor as bt
from constants import MAX_CIRCUIT_SIZE_GB


class CircuitValidator:
    REQUIRED_FILES = [
        "settings.json",
        "model.compiled",
    ]

    REQUIRED_SETTINGS = {
        "run_args": {
            "input_visibility": "Private",
            "output_visibility": "Public",
            "param_visibility": "Private",
            "commitment": "KZG",
        }
    }

    @classmethod
    def validate_files(cls, circuit_dir: str) -> bool:
        try:
            if not cls._validate_size(circuit_dir):
                return False

            if not cls._validate_required_files(circuit_dir):
                return False

            if not cls._validate_settings(circuit_dir):
                return False

            return True

        except Exception as e:
            bt.logging.error(f"Error validating circuit files: {e}")
            return False

    @classmethod
    def _validate_size(cls, circuit_dir: str) -> bool:
        total_size = sum(
            os.path.getsize(os.path.join(circuit_dir, f))
            for f in os.listdir(circuit_dir)
            if os.path.isfile(os.path.join(circuit_dir, f))
        )
        if total_size > MAX_CIRCUIT_SIZE_GB * 1024 * 1024 * 1024:
            bt.logging.error(
                f"Circuit files too large: {total_size / (1024 * 1024 * 1024):.2f} GB"
            )
            return False
        return True

    @classmethod
    def _validate_required_files(cls, circuit_dir: str) -> bool:
        for f in cls.REQUIRED_FILES:
            if not os.path.exists(os.path.join(circuit_dir, f)):
                bt.logging.error(f"Missing required file: {f}")
                return False
        return True

    @classmethod
    def _validate_settings(cls, circuit_dir: str) -> bool:
        try:
            with open(os.path.join(circuit_dir, "settings.json")) as f:
                settings = json.load(f)
                if "run_args" not in settings:
                    bt.logging.error("Missing run_args in settings.json")
                    return False

                run_args = settings["run_args"]
                required_args = cls.REQUIRED_SETTINGS["run_args"]

                for key, value in required_args.items():
                    if key not in run_args:
                        bt.logging.error(f"Missing required run_args setting: {key}")
                        return False
                    if run_args[key] != value:
                        bt.logging.error(
                            f"Invalid value for {key}: expected {value}, got {run_args[key]}"
                        )
                        return False

            return True
        except json.JSONDecodeError:
            bt.logging.error("Invalid JSON in settings.json")
            return False
