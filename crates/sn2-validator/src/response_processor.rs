use std::time::Instant;

use anyhow::Result;
use sn2_types::{MinerResponse, ProofSystem};
use tracing::warn;

use crate::dsperse::DSperseManager;

pub struct ResponseProcessor;

impl ResponseProcessor {
    pub fn new() -> Self {
        Self
    }

    pub async fn verify_response(
        &self,
        response: &mut MinerResponse,
        dsperse: &DSperseManager,
    ) -> Result<bool> {
        if response.proof_content.is_none() {
            anyhow::bail!("empty proof from miner {}", response.uid);
        }

        let start = Instant::now();
        let result = if response.is_incremental {
            self.verify_incremental(response, dsperse).await
        } else {
            self.verify_standard(response).await
        };
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
                warn!(
                    uid = response.uid,
                    "no circuit data for standard verification, accepting proof"
                );
                return Ok(!proof_hex.is_empty());
            }
        };

        if circuit.proof_system != ProofSystem::JSTPROVE {
            return Ok(!proof_hex.is_empty());
        }

        let circuit_path = circuit.paths.compiled_model.to_string_lossy().to_string();
        if !circuit.paths.compiled_model.exists() {
            warn!(uid = response.uid, path = %circuit_path, "compiled model not found, accepting proof");
            return Ok(!proof_hex.is_empty());
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
                warn!(uid = response.uid, error = %e, "standard verification failed");
                Ok(false)
            }
        }
    }

    async fn verify_incremental(
        &self,
        response: &mut MinerResponse,
        dsperse: &DSperseManager,
    ) -> Result<bool> {
        let witness_hex = match &response.witness {
            Some(w) if !w.is_empty() => w.clone(),
            _ => {
                anyhow::bail!(
                    "incremental response from miner {} missing witness",
                    response.uid
                );
            }
        };

        let proof_hex = response
            .proof_content
            .as_ref()
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if proof_hex.is_empty() {
            return Ok(false);
        }

        let circuit = match &response.circuit {
            Some(c) => c,
            None => {
                warn!(
                    uid = response.uid,
                    "no circuit data for incremental verification"
                );
                return Ok(false);
            }
        };

        let slice_num = response.dsperse_slice_num.unwrap_or(0).to_string();
        let inputs = response
            .inputs
            .as_ref()
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let proof_system_str = response.proof_system.as_ref().map(|ps| ps.to_string());

        match dsperse
            .verify_incremental_slice_with_witness(
                &circuit.id,
                &slice_num,
                &inputs,
                &witness_hex,
                proof_hex,
                proof_system_str.as_deref(),
                response.dsperse_run_uid.as_deref(),
            )
            .await
        {
            Ok((success, extracted_outputs)) => {
                if success {
                    if let Some(outputs) = extracted_outputs {
                        response.computed_outputs = Some(outputs);
                    }
                }
                Ok(success)
            }
            Err(e) => {
                warn!(uid = response.uid, error = %e, "incremental verification IPC failed");
                Ok(false)
            }
        }
    }
}
