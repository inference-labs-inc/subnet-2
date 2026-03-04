use serde::{Deserialize, Serialize};

use crate::{Circuit, ProofSystem, RequestType, RunSource};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub circuit: Circuit,
    pub inputs: serde_json::Value,
    pub request_type: RequestType,
    pub retry_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DSliceRequest {
    pub circuit: Circuit,
    pub inputs: serde_json::Value,
    pub request_type: RequestType,
    pub proof_system: ProofSystem,
    pub slice_num: String,
    pub run_uid: String,
    pub outputs: Option<serde_json::Value>,
    pub is_tile: bool,
    pub tile_idx: Option<u32>,
    pub task_id: Option<String>,
    pub run_source: RunSource,
    pub retry_count: u32,
    pub circuit_path: Option<String>,
}
