use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sn2_types::json_tensor::flatten_json_to_f64;
use tracing::info;

pub struct DSperseClient {
    cache_dir: PathBuf,
}

fn find_slice_onnx(slice_dir: &Path) -> Result<PathBuf> {
    let payload_dir = slice_dir.join("payload");
    if payload_dir.is_dir() {
        let mut candidates: Vec<PathBuf> = std::fs::read_dir(&payload_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "onnx"))
            .collect();
        candidates.sort();
        match candidates.len() {
            1 => return Ok(candidates.remove(0)),
            n if n > 1 => anyhow::bail!(
                "multiple .onnx files in {}: {:?}",
                payload_dir.display(),
                candidates,
            ),
            _ => {}
        }
    }
    anyhow::bail!("no .onnx file found in {}", payload_dir.display())
}

fn extract_input_json(inputs: &serde_json::Value) -> &serde_json::Value {
    for key in &["input_data", "input", "data", "inputs"] {
        if let Some(v) = inputs.get(*key) {
            return v;
        }
    }
    inputs
}

impl DSperseClient {
    pub fn new() -> Self {
        let cache_dir = PathBuf::from(shellexpand::tilde(sn2_types::CIRCUIT_CACHE_DIR).to_string());
        info!(cache_dir = %cache_dir.display(), "initialized DSperseClient");
        Self { cache_dir }
    }

    pub async fn prove_slice(
        &self,
        circuit_id: &str,
        slice_num: &str,
        inputs: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let slices_dir = self
            .cache_dir
            .join(format!("model_{circuit_id}"))
            .join("slices");
        let slice_idx: usize = slice_num
            .strip_prefix("slice_")
            .unwrap_or(slice_num)
            .parse()
            .context("parsing slice_num")?;

        let slice_dir = dsperse::utils::paths::slice_dir_path(&slices_dir, slice_idx);
        let circuit_path = slice_dir.join("jstprove").join("circuit.msgpack");
        let onnx_path = find_slice_onnx(&slice_dir)?;

        anyhow::ensure!(
            circuit_path.exists(),
            "circuit not found at {}",
            circuit_path.display()
        );
        anyhow::ensure!(
            onnx_path.exists(),
            "onnx model not found at {}",
            onnx_path.display()
        );

        info!(
            circuit_id,
            slice = slice_num,
            circuit_path = %circuit_path.display(),
            "generating witness and proof"
        );

        let input_data = extract_input_json(inputs).clone();

        tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
            let input_flat = flatten_json_to_f64(&input_data);
            anyhow::ensure!(
                !input_flat.is_empty(),
                "invalid input tensor: flattened input is empty"
            );

            let backend = dsperse::backend::jstprove::JstproveBackend::new();
            let params = backend
                .load_params(&circuit_path)
                .map_err(|e| anyhow::anyhow!("loading circuit params: {e}"))?;
            let is_wai = params.as_ref().is_some_and(|p| p.weights_as_inputs);

            let inits = if is_wai {
                dsperse::pipeline::extract_onnx_initializers(&onnx_path, params.as_ref().unwrap())
                    .map_err(|e| anyhow::anyhow!("extracting initializers: {e}"))?
            } else {
                Vec::new()
            };

            let witness_bytes = backend
                .witness_f64(&circuit_path, &input_flat, &inits)
                .map_err(|e| anyhow::anyhow!("witness generation: {e}"))?;

            let proof_bytes = backend
                .prove(&circuit_path, &witness_bytes)
                .map_err(|e| anyhow::anyhow!("proof generation: {e}"))?;

            let computed_outputs = if let Some(ref p) = params {
                let num_model_inputs = p.effective_input_dims();
                backend
                    .extract_outputs(&witness_bytes, num_model_inputs)
                    .map_err(|e| anyhow::anyhow!("extracting outputs: {e}"))?
            } else {
                Vec::new()
            };

            info!(
                witness_size = witness_bytes.len(),
                proof_size = proof_bytes.len(),
                num_outputs = computed_outputs.len(),
                "witness and proof generated"
            );

            Ok(serde_json::json!({
                "proof": hex::encode(&proof_bytes),
                "witness": hex::encode(&witness_bytes),
                "computed_outputs": computed_outputs,
            }))
        })
        .await
        .context("blocking task panicked")?
    }

    pub async fn prove(
        &self,
        model_id: &str,
        inputs: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let model_dir = self.cache_dir.join(format!("model_{model_id}"));
        let circuit_path = model_dir.join("model.compiled");

        anyhow::ensure!(
            circuit_path.exists(),
            "compiled model not found at {}",
            circuit_path.display()
        );

        info!(
            model_id,
            circuit_path = %circuit_path.display(),
            "generating witness and proof"
        );

        let inputs_clone = inputs.clone();

        tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
            let inputs_bytes = rmp_serde::to_vec_named(&inputs_clone)?;
            let backend = dsperse::backend::jstprove::JstproveBackend::new();

            let params = backend
                .load_params(&circuit_path)
                .map_err(|e| anyhow::anyhow!("loading circuit params: {e}"))?;

            let witness_bytes = backend
                .witness(&circuit_path, &inputs_bytes, &[])
                .map_err(|e| anyhow::anyhow!("witness generation: {e}"))?;

            let proof_bytes = backend
                .prove(&circuit_path, &witness_bytes)
                .map_err(|e| anyhow::anyhow!("proof generation: {e}"))?;

            let computed_outputs = if let Some(ref p) = params {
                let num_model_inputs = p.effective_input_dims();
                backend
                    .extract_outputs(&witness_bytes, num_model_inputs)
                    .map_err(|e| anyhow::anyhow!("extracting outputs: {e}"))?
            } else {
                Vec::new()
            };

            Ok(serde_json::json!({
                "proof": hex::encode(&proof_bytes),
                "witness": hex::encode(&witness_bytes),
                "computed_outputs": computed_outputs,
            }))
        })
        .await
        .context("blocking task panicked")?
    }
}
