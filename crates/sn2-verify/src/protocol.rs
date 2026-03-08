use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ServiceRequest {
    #[serde(rename = "verify")]
    Verify(VerifyRequest),
    #[serde(rename = "verify_and_store")]
    VerifyAndStore(VerifyAndStoreRequest),
    #[serde(rename = "store")]
    Store(StoreRequest),
    #[serde(rename = "reconstruct")]
    Reconstruct(ReconstructRequest),
    #[serde(rename = "evict")]
    Evict(EvictRequest),
}

#[derive(Debug, Deserialize)]
pub struct VerifyRequest {
    pub request_id: String,
    pub circuit_path: String,
    pub witness_hex: String,
    pub proof_hex: String,
    pub num_inputs: usize,
    pub expected_inputs: Option<Vec<f64>>,
    #[serde(default = "default_pcs_type")]
    pub pcs_type: String,
}

#[derive(Debug, Deserialize)]
pub struct VerifyAndStoreRequest {
    pub request_id: String,
    pub circuit_path: String,
    pub witness_hex: String,
    pub proof_hex: String,
    pub num_inputs: usize,
    pub expected_inputs: Option<Vec<f64>>,
    #[serde(default = "default_pcs_type")]
    pub pcs_type: String,
    pub store_key: String,
    pub output_shape: [usize; 4],
}

#[derive(Debug, Deserialize)]
pub struct ReconstructRequest {
    pub tile_keys: Vec<String>,
    pub tiles_y: usize,
    pub tiles_x: usize,
}

#[derive(Debug, Deserialize)]
pub struct StoreRequest {
    pub store_key: String,
    pub data: Vec<f64>,
    pub channels: usize,
    pub height: usize,
    pub width: usize,
}

#[derive(Debug, Deserialize)]
pub struct EvictRequest {
    pub keys: Vec<String>,
}

fn default_pcs_type() -> String {
    "Hyrax".to_string()
}

#[derive(Debug, Serialize)]
pub struct VerifyResponse {
    pub request_id: String,
    pub success: bool,
    pub rescaled_outputs: Option<Vec<f64>>,
    pub scale_base: Option<u64>,
    pub scale_exponent: Option<u64>,
    pub error: Option<String>,
}

impl VerifyResponse {
    pub fn error(request_id: String, msg: String) -> Self {
        Self {
            request_id,
            success: false,
            rescaled_outputs: None,
            scale_base: None,
            scale_exponent: None,
            error: Some(msg),
        }
    }

    pub fn ok(
        request_id: String,
        rescaled_outputs: Vec<f64>,
        scale_base: u64,
        scale_exponent: u64,
    ) -> Self {
        Self {
            request_id,
            success: true,
            rescaled_outputs: Some(rescaled_outputs),
            scale_base: Some(scale_base),
            scale_exponent: Some(scale_exponent),
            error: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct StoreResponse {
    pub request_id: String,
    pub success: bool,
    pub stored: bool,
    pub error: Option<String>,
}

impl StoreResponse {
    pub fn ok(request_id: String) -> Self {
        Self {
            request_id,
            success: true,
            stored: true,
            error: None,
        }
    }

    pub fn error(request_id: String, msg: String) -> Self {
        Self {
            request_id,
            success: false,
            stored: false,
            error: Some(msg),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ReconstructResponse {
    pub success: bool,
    pub output: Option<Vec<f64>>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EvictResponse {
    pub success: bool,
    pub evicted: usize,
}
