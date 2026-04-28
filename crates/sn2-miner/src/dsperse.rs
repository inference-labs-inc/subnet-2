use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sn2_types::json_tensor::flatten_json_to_f64;
use tracing::info;

pub struct DSperseClient {
    cache_dir: PathBuf,
}

fn validate_circuit_id(id: &str) -> Result<()> {
    anyhow::ensure!(
        id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid circuit id: expected 64-char hex string"
    );
    Ok(())
}

pub fn normalize_slice_id(slice_num: &str) -> Result<String> {
    let idx: usize = slice_num
        .strip_prefix("slice_")
        .unwrap_or(slice_num)
        .parse()
        .context("parsing slice_num")?;
    Ok(format!("slice_{idx}"))
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

fn prove_and_build_response(
    backend: &dsperse::backend::jstprove::JstproveBackend,
    circuit_path: &Path,
    witness_bytes: &[u8],
    effective_input_dims: Option<usize>,
) -> Result<serde_json::Value> {
    let holographic = circuit_path.join("vk.bin").is_file();
    let proof_bytes = if holographic {
        backend
            .prove_holographic(circuit_path, witness_bytes)
            .map_err(|e| anyhow::anyhow!("holographic proof generation: {e}"))?
    } else {
        backend
            .prove(circuit_path, witness_bytes)
            .map_err(|e| anyhow::anyhow!("proof generation: {e}"))?
    };

    let computed_outputs = if let Some(num_model_inputs) = effective_input_dims {
        match backend.extract_outputs(witness_bytes, num_model_inputs) {
            Ok(outputs) => outputs,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    num_model_inputs,
                    witness_len = witness_bytes.len(),
                    "output extraction failed; validator will extract from verified proof"
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    info!(
        witness_size = witness_bytes.len(),
        proof_size = proof_bytes.len(),
        num_outputs = computed_outputs.len(),
        holographic,
        "witness and proof generated"
    );

    Ok(serde_json::json!({
        "proof": hex::encode(&proof_bytes),
        "witness": hex::encode(witness_bytes),
        "computed_outputs": computed_outputs,
    }))
}

impl DSperseClient {
    pub fn new() -> Self {
        let cache_dir = PathBuf::from(shellexpand::tilde(sn2_types::CIRCUIT_CACHE_DIR).to_string());
        info!(cache_dir = %cache_dir.display(), "initialized DSperseClient");
        Self { cache_dir }
    }

    pub async fn resolve_component(
        &self,
        component_sha: &str,
        slice_id: &str,
    ) -> Result<Option<PathBuf>> {
        let cache_dir = self.cache_dir.clone();
        let component_sha = component_sha.to_string();
        let slice_id = slice_id.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<PathBuf>> {
            let entries = match std::fs::read_dir(&cache_dir) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "reading cache directory {}: {e}",
                        cache_dir.display()
                    ))
                }
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if !name_str.starts_with("model_") {
                    continue;
                }
                let stamp_path = entry
                    .path()
                    .join("slices")
                    .join(&slice_id)
                    .join("component.sha");
                if let Ok(stamp) = std::fs::read_to_string(&stamp_path) {
                    if stamp.trim() == component_sha {
                        let slice_dir = entry.path().join("slices").join(&slice_id);
                        if !slice_dir.join("jstprove").join("circuit.bundle").is_dir() {
                            continue;
                        }
                        if find_slice_onnx(&slice_dir).is_err() {
                            continue;
                        }
                        return Ok(Some(slice_dir));
                    }
                }
            }
            Ok(None)
        })
        .await
        .context("component resolution task panicked")?
    }

    pub async fn prove(
        &self,
        model_id: &str,
        inputs: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        validate_circuit_id(model_id)?;
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
            "generating witness and proof for non-composable model"
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

            let dims = params.as_ref().map(|p| p.effective_input_dims());
            prove_and_build_response(&backend, &circuit_path, &witness_bytes, dims)
        })
        .await
        .context("blocking task panicked")?
    }

    pub async fn prove_slice(
        &self,
        circuit_id: &str,
        slice_num: &str,
        inputs: &serde_json::Value,
        resolved_component_dir: PathBuf,
    ) -> Result<serde_json::Value> {
        validate_circuit_id(circuit_id)?;
        // Validate slice format; the normalized path is not needed since
        // resolved_component_dir already contains the canonical slice path.
        let _ = normalize_slice_id(slice_num)?;

        let slice_dir = resolved_component_dir;

        anyhow::ensure!(
            slice_dir.is_dir(),
            "resolved component directory not found at {}",
            slice_dir.display()
        );

        let circuit_path = slice_dir.join("jstprove").join("circuit.bundle");
        let onnx_path = find_slice_onnx(&slice_dir)?;

        anyhow::ensure!(
            circuit_path.is_dir(),
            "bundle directory not found at {}",
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
            let inits = match params.as_ref() {
                Some(p) if p.weights_as_inputs => {
                    dsperse::pipeline::extract_onnx_initializers(&onnx_path, p)
                        .map_err(|e| anyhow::anyhow!("extracting initializers: {e}"))?
                }
                _ => Vec::new(),
            };

            let witness_bytes = backend
                .witness_f64(&circuit_path, &input_flat, &inits)
                .map_err(|e| anyhow::anyhow!("witness generation: {e}"))?;

            let dims = params.as_ref().map(|p| p.effective_input_dims());
            prove_and_build_response(&backend, &circuit_path, &witness_bytes, dims)
        })
        .await
        .context("blocking task panicked")?
    }
}
