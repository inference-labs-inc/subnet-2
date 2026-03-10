use serde::{Deserialize, Serialize};

use crate::ProofSystem;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryZkProof {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_output: Option<String>,
}

impl QueryZkProof {
    pub const NAME: &str = "query-zk-proof";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofOfWeightsDataModel {
    #[serde(default = "default_subnet_uid")]
    pub subnet_uid: i32,
    pub verification_key_hash: String,
    #[serde(default = "default_pow_proof_system")]
    pub proof_system: ProofSystem,
    pub inputs: serde_json::Value,
    pub proof: String,
    pub public_signals: String,
}

impl ProofOfWeightsDataModel {
    pub const NAME: &str = "proof-of-weights";
}

fn default_subnet_uid() -> i32 {
    2
}

fn default_pow_proof_system() -> ProofSystem {
    ProofSystem::CIRCOM
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DSliceProofGenerationDataModel {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub circuit: Option<String>,
    #[serde(default = "default_dslice_proof_system")]
    pub proof_system: ProofSystem,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slice_num: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_uid: Option<String>,
}

impl DSliceProofGenerationDataModel {
    pub const NAME: &str = "dsperse-proof-generation";
}

fn default_dslice_proof_system() -> ProofSystem {
    ProofSystem::JSTPROVE
}
