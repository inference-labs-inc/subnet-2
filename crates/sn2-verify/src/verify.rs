use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result};
use tracing::{info, warn};

use jstprove_circuits::onnx::verify_and_extract_bn254;
use jstprove_circuits::runner::main_runner::read_circuit_msgpack;

static CIRCUIT_CACHE: LazyLock<Mutex<HashMap<String, Arc<Vec<u8>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

use crate::protocol::{StoreResponse, VerifyAndStoreRequest, VerifyRequest, VerifyResponse};
use crate::store::{StoredTile, TileStore};

pub fn evict_circuit_cache(path_prefix: &str) {
    let mut cache = CIRCUIT_CACHE.lock().unwrap();
    let before = cache.len();
    cache.retain(|k, _| !k.starts_with(path_prefix));
    let evicted = before - cache.len();
    if evicted > 0 {
        info!(prefix = %path_prefix, evicted, remaining = cache.len(), "evicted circuit cache entries");
    }
}

pub struct VerifyResult {
    pub rescaled_outputs: Vec<f64>,
    pub scale_base: u64,
    pub scale_exponent: u64,
}

pub async fn verify_inner(
    _request_id: &str,
    circuit_path: &str,
    witness_hex: &str,
    proof_hex: &str,
    num_inputs: usize,
    expected_inputs: &Option<Vec<f64>>,
    _pcs_type: &str,
) -> Result<VerifyResult> {
    let circuit_path = circuit_path.to_string();
    let witness_hex = witness_hex.to_string();
    let proof_hex = proof_hex.to_string();
    let expected_inputs = expected_inputs.clone();

    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let result = (|| -> Result<VerifyResult> {
            let circuit_bytes = {
                let mut cache = CIRCUIT_CACHE.lock().unwrap();
                if let Some(cached) = cache.get(&circuit_path) {
                    Arc::clone(cached)
                } else {
                    let bytes = if std::path::Path::new(&circuit_path).is_dir() {
                        let bundle = read_circuit_msgpack(&circuit_path)
                            .map_err(|e| anyhow::anyhow!("reading bundle {circuit_path}: {e}"))?;
                        bundle.circuit
                    } else {
                        std::fs::read(&circuit_path)
                            .with_context(|| format!("reading {circuit_path}"))?
                    };
                    info!(path = %circuit_path, size_mb = bytes.len() as f64 / (1024.0 * 1024.0), "cached circuit bytes");
                    let arc = Arc::new(bytes);
                    cache.insert(circuit_path.clone(), Arc::clone(&arc));
                    arc
                }
            };
            let witness_bytes = hex::decode(witness_hex.trim()).context("hex-decoding witness")?;
            let proof_bytes = hex::decode(proof_hex.trim()).context("hex-decoding proof")?;

            let result = verify_and_extract_bn254(
                &circuit_bytes,
                &witness_bytes,
                &proof_bytes,
                num_inputs,
                expected_inputs.as_deref(),
            )
            .map_err(|e| anyhow::anyhow!("verification: {e}"))?;

            if !result.valid {
                anyhow::bail!("proof verification failed");
            }

            Ok(VerifyResult {
                rescaled_outputs: result.outputs,
                scale_base: result.scale_base,
                scale_exponent: result.scale_exponent,
            })
        })();
        let _ = tx.send(result);
    });
    rx.await.context("verification thread panicked")?
}

pub async fn handle_request(req: VerifyRequest) -> VerifyResponse {
    match verify_inner(
        &req.request_id,
        &req.circuit_path,
        &req.witness_hex,
        &req.proof_hex,
        req.num_inputs,
        &req.expected_inputs,
        &req.pcs_type,
    )
    .await
    {
        Ok(result) => VerifyResponse::ok(
            req.request_id,
            result.rescaled_outputs,
            result.scale_base,
            result.scale_exponent,
        ),
        Err(e) => {
            warn!(request_id = %req.request_id, error = %e, "verification failed");
            VerifyResponse::error(req.request_id, format!("{e:#}"))
        }
    }
}

pub async fn handle_store_request(
    req: VerifyAndStoreRequest,
    store: &Arc<TileStore>,
) -> StoreResponse {
    match verify_inner(
        &req.request_id,
        &req.circuit_path,
        &req.witness_hex,
        &req.proof_hex,
        req.num_inputs,
        &req.expected_inputs,
        &req.pcs_type,
    )
    .await
    {
        Ok(result) => {
            let [_, channels, height, width] = req.output_shape;
            let expected_len = match channels
                .checked_mul(height)
                .and_then(|v| v.checked_mul(width))
            {
                Some(len) => len,
                None => {
                    return StoreResponse::error(
                        req.request_id,
                        format!(
                            "output shape dimensions overflow: {}x{}x{}",
                            channels, height, width
                        ),
                    );
                }
            };
            if result.rescaled_outputs.len() != expected_len {
                return StoreResponse::error(
                    req.request_id,
                    format!(
                        "output length {} != expected {} (shape {:?})",
                        result.rescaled_outputs.len(),
                        expected_len,
                        req.output_shape
                    ),
                );
            }
            let store_key = req.store_key;
            if let Err(e) = store.insert(
                store_key,
                StoredTile {
                    data: result.rescaled_outputs,
                    channels,
                    height,
                    width,
                },
            ) {
                return StoreResponse::error(req.request_id, format!("tile store insert: {e:#}"));
            }
            StoreResponse::ok(req.request_id)
        }
        Err(e) => {
            warn!(request_id = %req.request_id, error = %e, "verify_and_store failed");
            StoreResponse::error(req.request_id, format!("{e:#}"))
        }
    }
}
