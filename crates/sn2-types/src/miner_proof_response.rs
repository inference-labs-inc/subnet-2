use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QueryZkProofResponse {
    #[serde(default, with = "serde_bytes")]
    pub query_output: Vec<u8>,
    #[serde(default, with = "serde_bytes")]
    pub witness: Vec<u8>,
    #[serde(default)]
    pub computed_outputs: Vec<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProofOfWeightsResponse {
    #[serde(default, with = "serde_bytes")]
    pub proof: Vec<u8>,
    #[serde(default, with = "serde_bytes")]
    pub public_signals: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DSliceResponse {
    #[serde(default, with = "serde_bytes")]
    pub proof: Vec<u8>,
    #[serde(default, with = "serde_bytes")]
    pub witness: Vec<u8>,
    #[serde(default)]
    pub computed_outputs: Vec<f64>,
}
