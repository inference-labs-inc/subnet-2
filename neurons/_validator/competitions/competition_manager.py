from __future__ import annotations
import time
from typing import Optional, Dict, Any
import bittensor as bt
from pydantic import BaseModel
from datetime import datetime
import json
import os
import enum
import cli_parser
from utils.gc_logging import gc_log_competition_metrics


class CompetitionStatus(enum.Enum):
    PENDING = "Pending"
    ACTIVE = "Active"
    COMPLETED = "Completed"
    INACTIVE = "Inactive"


class CompetitionMetrics(BaseModel):
    """Competition metrics for logging"""

    competition_id: int
    name: str
    status: CompetitionStatus
    accuracy_weight: float
    active_participants: int
    total_circuits_evaluated: int
    avg_accuracy: float
    avg_proof_size: float
    avg_response_time: float
    sota_score: float
    sota_hotkey: Optional[str]
    sota_proof_size: float
    sota_response_time: float
    timestamp: int


class DataSourceConfig(BaseModel):
    type: str = "random"
    url: Optional[str] = None
    format: str = "npz"  # npz, zip, tar
    input_key: str = "inputs"  # For npz, or subdir name for zip/tar
    input_pattern: Optional[str] = None  # For matching files in zip/tar
    input_transform: Optional[str] = None  # resize, normalize, etc
    transform_params: Dict[str, Any] = {}


class CompetitionConfig(BaseModel):
    """Configuration for a competition"""

    id: int
    name: str
    description: str
    start_timestamp: int
    end_timestamp: int
    baseline_model_path: str
    max_accuracy_weight: float = 1.0
    min_accuracy_weight: float = 0.0
    data_source: Optional[DataSourceConfig] = None
    circuit_settings: Dict[str, Any] = {}


class CompetitionState(BaseModel):
    """Current state of a competition"""

    is_active: bool = False
    current_accuracy_weight: float = 0.0
    last_sync_timestamp: int = 0
    total_circuits_evaluated: int = 0
    active_participants: int = 0
    current_metrics: Optional[CompetitionMetrics] = None


class CompetitionManager:
    """Manages competition lifecycle and state"""

    def __init__(self, config_dir: str):
        self.config_dir = config_dir
        self.state_file = os.path.join(config_dir, "competition_state.json")
        self.config_file = os.path.join(config_dir, "competition_config.json")
        self.current_competition: Optional[CompetitionConfig] = None
        self.state = CompetitionState()
        self.wallet = bt.wallet(config=cli_parser.config)

        self._load_state()
        self._load_config()

    def _load_state(self):
        """Load competition state from disk"""
        try:
            if os.path.exists(self.state_file):
                with open(self.state_file, "r") as f:
                    data = json.load(f)
                    self.state = CompetitionState(**data)
        except Exception as e:
            bt.logging.error(f"Error loading competition state: {e}")
            self.state = CompetitionState()

    def _save_state(self):
        """Save current competition state to disk"""
        try:
            with open(self.state_file, "w") as f:
                json.dump(self.state.dict(), f, indent=4)
        except Exception as e:
            bt.logging.error(f"Error saving competition state: {e}")

    def _load_config(self):
        """Load competition config from disk"""
        try:
            if os.path.exists(self.config_file):
                with open(self.config_file, "r") as f:
                    data = json.load(f)
                    self.current_competition = CompetitionConfig(**data)
        except Exception as e:
            bt.logging.error(f"Error loading competition config: {e}")
            self.current_competition = None

    def update_competition_state(self) -> None:
        """Update competition state based on current time"""
        if not self.current_competition:
            return

        current_time = int(time.time())

        is_active = (
            self.current_competition.start_timestamp
            <= current_time
            <= self.current_competition.end_timestamp
        )

        if is_active:
            accuracy_weight = self.current_competition.max_accuracy_weight
        else:
            accuracy_weight = self.current_competition.min_accuracy_weight

        self.state.is_active = is_active
        self.state.current_accuracy_weight = accuracy_weight
        self.state.last_sync_timestamp = current_time

        self._save_state()

    def get_accuracy_weight(self) -> float:
        """Get current accuracy weight for scoring"""
        return self.state.current_accuracy_weight

    def is_competition_active(self) -> bool:
        """Check if competition is currently active"""
        return self.state.is_active

    def get_competition_status(self) -> dict:
        """Get current competition status"""
        if not self.current_competition:
            return {"status": CompetitionStatus.INACTIVE}

        current_time = int(time.time())

        if current_time < self.current_competition.start_timestamp:
            time_to_start = self.current_competition.start_timestamp - current_time
            return {
                "status": CompetitionStatus.PENDING,
                "competition_id": self.current_competition.id,
                "name": self.current_competition.name,
                "starts_in": f"{time_to_start // 3600} hours",
                "accuracy_weight": self.state.current_accuracy_weight,
            }
        elif current_time > self.current_competition.end_timestamp:
            return {
                "status": CompetitionStatus.COMPLETED,
                "competition_id": self.current_competition.id,
                "name": self.current_competition.name,
                "ended": datetime.fromtimestamp(
                    self.current_competition.end_timestamp
                ).isoformat(),
                "accuracy_weight": self.state.current_accuracy_weight,
            }
        else:
            time_remaining = self.current_competition.end_timestamp - current_time
            return {
                "status": CompetitionStatus.ACTIVE,
                "competition_id": self.current_competition.id,
                "name": self.current_competition.name,
                "time_remaining": f"{time_remaining // 3600} hours",
                "accuracy_weight": self.state.current_accuracy_weight,
            }

    def log_metrics(self, metrics: dict):
        """Log competition metrics."""
        if not self.current_competition or not self.is_competition_active():
            return

        try:

            comp_metrics = {
                "competition_id": self.current_competition.id,
                "name": self.current_competition.name,
                "status": self.get_competition_status()["status"].value,
                "accuracy_weight": self.state.current_accuracy_weight,
                "total_circuits_evaluated": self.state.total_circuits_evaluated,
                "timestamp": int(time.time()),
                **metrics,
            }

            gc_log_competition_metrics(comp_metrics, self.wallet.hotkey)
        except Exception as e:
            bt.logging.error(f"Error logging metrics: {e}")

    def increment_circuits_evaluated(self):
        """Increment the total circuits evaluated counter"""
        self.state.total_circuits_evaluated += 1
        self._save_state()

    def update_active_participants(self, count: int):
        """Update the count of active participants"""
        self.state.active_participants = count
        self._save_state()
