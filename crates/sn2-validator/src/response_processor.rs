use std::time::Instant;

use anyhow::Result;
use sn2_types::{Circuit, MinerResponse, ProofSystem};
use tracing::warn;

fn resolve_circuit_path(response: &MinerResponse, circuit: &Circuit) -> String {
    if let Some(ref p) = response.dsperse_circuit_path {
        return p.clone();
    }

    if response.is_incremental {
        let slice_num = response.dsperse_slice_num.unwrap_or(0);
        let slice_model = circuit
            .paths
            .base_path
            .join("slices")
            .join(format!("slice_{slice_num}"))
            .join("model.compiled");
        if slice_model.exists() {
            return slice_model.to_string_lossy().to_string();
        }
    }

    circuit.paths.compiled_model.to_string_lossy().to_string()
}

fn resolve_num_inputs(circuit: &Circuit, inputs: &Option<serde_json::Value>) -> usize {
    circuit
        .settings
        .get("num_inputs")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(|| {
            inputs
                .as_ref()
                .and_then(|v| v.get("input_data"))
                .map(|v| sn2_types::json_tensor::flatten_json_to_f64(v).len())
                .unwrap_or(0)
        })
}

fn compute_slice_stats(data: &[f64]) -> (f64, usize, usize, usize) {
    let max_abs = data.iter().map(|x| x.abs()).fold(0.0_f64, f64::max);
    let inf_count = data.iter().filter(|x| x.is_infinite()).count();
    let nan_count = data.iter().filter(|x| x.is_nan()).count();
    let f32_overflow = data.iter().filter(|&&x| x.abs() > f32::MAX as f64).count();
    (max_abs, inf_count, nan_count, f32_overflow)
}

fn compute_output_stats(value: &serde_json::Value) -> Option<(usize, f64, usize, usize, usize)> {
    let flat = sn2_types::json_tensor::flatten_json_to_f64(value);
    if flat.is_empty() {
        return None;
    }
    let (max_abs, inf_count, nan_count, f32_overflow) = compute_slice_stats(&flat);
    Some((flat.len(), max_abs, inf_count, nan_count, f32_overflow))
}

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

        let circuit_path = resolve_circuit_path(response, circuit);

        if !std::path::Path::new(&circuit_path).exists() {
            warn!(uid = response.uid, path = %circuit_path, "compiled model not found");
            return Ok(false);
        }

        let num_inputs = resolve_num_inputs(circuit, &response.inputs);

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

        let miner_outputs_stats = response
            .computed_outputs
            .as_ref()
            .and_then(compute_output_stats);

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
                let (max_abs, inf_count, nan_count, f32_overflow) =
                    compute_slice_stats(&result.rescaled_outputs);
                let slice_num = response.dsperse_slice_num;
                let sample: Vec<f64> = result.rescaled_outputs.iter().copied().take(8).collect();
                tracing::debug!(
                    uid = response.uid,
                    slice = ?slice_num,
                    num_inputs,
                    circuit_path = %circuit_path,
                    scale_base = result.scale_base,
                    scale_exponent = result.scale_exponent,
                    num_outputs = result.rescaled_outputs.len(),
                    max_abs,
                    inf_count,
                    nan_count,
                    f32_overflow,
                    miner_stats = ?miner_outputs_stats,
                    sample = ?sample,
                    "circuit output extraction diagnostics"
                );
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
