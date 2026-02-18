use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct VerifyRequest {
    pub request_id: String,
    pub circuit_path: String,
    pub witness_shm_path: String,
    pub proof_hex: String,
    pub num_inputs: usize,
    pub expected_inputs: Option<Vec<f64>>,
    #[serde(default = "default_pcs_type")]
    pub pcs_type: String,
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
