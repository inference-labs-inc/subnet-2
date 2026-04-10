use std::time::Instant;

use anyhow::Result;
use sn2_types::{Circuit, MinerResponse, ProofSystem};
use tracing::warn;

fn resolve_circuit_path(response: &MinerResponse, circuit: &Circuit) -> Option<String> {
    let slice_num = response.dsperse_slice_num.unwrap_or(0);
    let bundle = circuit
        .paths
        .base_path
        .join("slices")
        .join(format!("slice_{slice_num}"))
        .join("jstprove/circuit.bundle");
    if bundle.is_dir() {
        Some(bundle.to_string_lossy().to_string())
    } else {
        None
    }
}

struct CircuitContext {
    num_inputs: usize,
    weights_as_inputs: bool,
}

fn load_circuit_context(
    circuit: &Circuit,
    inputs: &Option<serde_json::Value>,
    circuit_path: &str,
) -> CircuitContext {
    let backend = dsperse::backend::jstprove::JstproveBackend::new();
    let params = backend
        .load_params(std::path::Path::new(circuit_path))
        .ok()
        .flatten();

    let num_inputs = circuit
        .settings
        .get("num_inputs")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .or_else(|| {
            params
                .as_ref()
                .map(|p| p.effective_input_dims())
                .filter(|d| *d > 0)
        })
        .unwrap_or_else(|| {
            inputs
                .as_ref()
                .and_then(|v| v.get("input_data"))
                .map(|v| sn2_types::json_tensor::flatten_json_to_f64(v).len())
                .unwrap_or(0)
        });

    let weights_as_inputs = params.as_ref().is_some_and(|p| p.weights_as_inputs);

    CircuitContext {
        num_inputs,
        weights_as_inputs,
    }
}

fn build_expected_inputs(
    activation_inputs: &Option<serde_json::Value>,
    ctx: &CircuitContext,
    circuit_path: &str,
) -> Option<Vec<f64>> {
    let mut flat: Vec<f64> = activation_inputs
        .as_ref()
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_f64()).collect())?;

    if flat.is_empty() {
        return None;
    }

    if !ctx.weights_as_inputs {
        return Some(flat);
    }

    let bundle_path = std::path::Path::new(circuit_path);
    let slice_dir = bundle_path.parent().and_then(|p| p.parent())?;
    let payload_dir = slice_dir.join("payload");

    let onnx_path = std::fs::read_dir(&payload_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "onnx"))?;

    let backend = dsperse::backend::jstprove::JstproveBackend::new();
    let params = backend
        .load_params(std::path::Path::new(circuit_path))
        .ok()
        .flatten()?;

    match dsperse::pipeline::extract_onnx_initializers(&onnx_path, &params) {
        Ok(inits) => {
            for (data, _shape) in inits {
                flat.extend(data);
            }
            Some(flat)
        }
        Err(e) => {
            tracing::warn!(
                onnx = %onnx_path.display(),
                error = %e,
                "failed to extract weight initializers for input verification"
            );
            Some(flat)
        }
    }
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

        let circuit_path = match resolve_circuit_path(response, circuit) {
            Some(p) => p,
            None => {
                let slice_num = response.dsperse_slice_num.unwrap_or(0);
                warn!(
                    uid = response.uid,
                    slice = slice_num,
                    "circuit bundle not found for slice"
                );
                return Ok(false);
            }
        };

        let ctx = load_circuit_context(circuit, &response.inputs, &circuit_path);
        let num_inputs = ctx.num_inputs;
        let expected_inputs = build_expected_inputs(&response.inputs, &ctx, &circuit_path);

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
                warn!(
                    uid = response.uid,
                    error = format!("{e:#}"),
                    "verification failed"
                );
                Ok(false)
            }
        }
    }
}
