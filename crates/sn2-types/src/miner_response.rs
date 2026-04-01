use serde::{Deserialize, Serialize};

use crate::{Circuit, ProofSystem, RequestType};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinerResponse {
    pub uid: u16,
    pub verification_result: bool,
    pub external_request_hash: String,
    pub response_time: f64,
    pub proof_size: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub circuit: Option<Circuit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof_system: Option<ProofSystem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof_content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_json: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_type: Option<RequestType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dsperse_slice_num: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dsperse_run_uid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub save: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub computed_outputs: Option<serde_json::Value>,
    #[serde(default)]
    pub is_incremental: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub witness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dsperse_circuit_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component_sha: Option<String>,
}
