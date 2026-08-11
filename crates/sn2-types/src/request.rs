use std::sync::Arc;

use bytes::Bytes;

use crate::{Circuit, ProofSystem, RequestType, RunSource};

#[derive(Debug, Clone)]
pub struct DSliceRequest {
    pub circuit: Arc<Circuit>,
    pub inputs: Bytes,
    pub request_type: RequestType,
    pub proof_system: ProofSystem,
    pub slice_num: String,
    pub run_uid: String,
    pub outputs: Option<Bytes>,
    pub is_tile: bool,
    pub tile_idx: Option<u32>,
    pub task_id: Option<String>,
    pub run_source: RunSource,
    pub retry_count: u32,
    // The uid whose dispatch of this request most recently failed. Retries
    // are not miner-affine, so without this a request rescheduled off a dead
    // or broken miner can burn its entire retry budget against that same
    // peer before any other miner gets a slot for it.
    pub last_failed_uid: Option<u16>,
    pub circuit_path: Option<String>,
    pub component_sha: Option<String>,
}
