use std::time::Instant;

use anyhow::Result;
use sn2_types::{MinerResponse, ProofSystem};
use tracing::warn;

pub struct ResponseProcessor;

impl ResponseProcessor {
    pub fn new() -> Self {
        Self
    }

    pub async fn verify_response(&self, response: &mut MinerResponse) -> Result<bool> {
        if response.proof_content.is_none() {
            anyhow::bail!("empty proof from miner {}", response.uid);
        }

        let start = Instant::now();
        let result = self.verify_standard(response).await;
        response.verification_time = Some(start.elapsed().as_secs_f64());
        result
    }

    async fn verify_standard(&self, response: &mut MinerResponse) -> Result<bool> {
        let proof_hex = response
            .proof_content
            .as_ref()
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if proof_hex.is_empty() {
            return Ok(false);
        }

        let witness_hex = match &response.witness {
            Some(w) if !w.is_empty() => w.as_str(),
            _ => return Ok(false),
        };

        let circuit = match &response.circuit {
            Some(c) => c,
            None => {
                warn!(uid = response.uid, "no circuit data for verification");
                return Ok(false);
            }
        };

        if circuit.proof_system != ProofSystem::JSTPROVE {
            warn!(uid = response.uid, proof_system = ?circuit.proof_system, "unsupported proof system");
            return Ok(false);
        }

        let circuit_path = if let Some(ref p) = response.dsperse_circuit_path {
            p.clone()
        } else if response.is_incremental {
            let slice_num = response.dsperse_slice_num.unwrap_or(0);
            let slice_model = circuit
                .paths
                .base_path
                .join("slices")
                .join(format!("slice_{slice_num}"))
                .join("model.compiled");
            if slice_model.exists() {
                slice_model.to_string_lossy().to_string()
            } else {
                circuit.paths.compiled_model.to_string_lossy().to_string()
            }
        } else {
            circuit.paths.compiled_model.to_string_lossy().to_string()
        };

        if !std::path::Path::new(&circuit_path).exists() {
            warn!(uid = response.uid, path = %circuit_path, "compiled model not found");
            return Ok(false);
        }

        let num_inputs = circuit
            .settings
            .get("num_inputs")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        let expected_inputs: Option<Vec<f64>> = response
            .inputs
            .as_ref()
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_f64()).collect());

        let pcs_type = circuit
            .settings
            .get("pcs_type")
            .and_then(|v| v.as_str())
            .unwrap_or("raw")
            .to_string();

        let request_id = format!("verify-{}", response.uid);

        match sn2_verify::verify_inner(
            &request_id,
            &circuit_path,
            witness_hex,
            proof_hex,
            num_inputs,
            &expected_inputs,
            &pcs_type,
        )
        .await
        {
            Ok(result) => {
                response.computed_outputs =
                    Some(serde_json::to_value(&result.rescaled_outputs).unwrap_or_default());
                Ok(true)
            }
            Err(e) => {
                warn!(uid = response.uid, error = %e, "verification failed");
                Ok(false)
            }
        }
    }
}
