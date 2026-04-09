use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use anyhow::{Context, Result};
use tracing::warn;

use dsperse::backend::jstprove::JstproveBackend;

static BACKEND: LazyLock<Arc<JstproveBackend>> = LazyLock::new(|| Arc::new(JstproveBackend::new()));

/// Monotonic counter bumped on every eviction or full clear of the
/// backend's bundle cache. In-flight verifications snapshot this
/// counter before running and refuse to return success if it has
/// changed by the time they finish, so a verification that was
/// reading a stale `Arc<CompiledCircuit>` while eviction happened
/// elsewhere is rejected instead of attesting against a circuit the
/// validator no longer trusts.
static EVICTION_GENERATION: AtomicU64 = AtomicU64::new(0);

use crate::protocol::{StoreResponse, VerifyAndStoreRequest, VerifyRequest, VerifyResponse};
use crate::store::{StoredTile, TileStore};

/// Evict cached bundles whose canonical path starts with the given prefix.
pub fn evict_circuit_cache(path_prefix: &str) {
    EVICTION_GENERATION.fetch_add(1, Ordering::SeqCst);
    BACKEND.evict_cache_by_prefix(std::path::Path::new(path_prefix));
}

/// Clear all cached bundles.
pub fn clear_circuit_cache() {
    EVICTION_GENERATION.fetch_add(1, Ordering::SeqCst);
    BACKEND.clear_cache();
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
    let circuit_path = std::path::PathBuf::from(circuit_path);
    let witness_hex = witness_hex.to_string();
    let proof_hex = proof_hex.to_string();
    let expected_inputs = expected_inputs.clone();
    let backend = Arc::clone(&*BACKEND);

    tokio::task::spawn_blocking(move || {
        let witness_bytes = hex::decode(witness_hex.trim()).context("hex-decoding witness")?;
        let proof_bytes = hex::decode(proof_hex.trim()).context("hex-decoding proof")?;

        // Snapshot the eviction generation before loading the bundle.
        // The dsperse backend hands out an Arc<CompiledCircuit> from
        // its cache, which keeps the in-memory bytes alive across
        // an eviction; we re-check the counter after verification
        // and refuse a positive result if eviction happened between
        // the two reads.
        let gen_before = EVICTION_GENERATION.load(Ordering::SeqCst);

        let verified = backend
            .verify_and_extract(
                &circuit_path,
                &witness_bytes,
                &proof_bytes,
                num_inputs,
                expected_inputs.as_deref(),
            )
            .context("verification")?;

        let gen_after = EVICTION_GENERATION.load(Ordering::SeqCst);
        if gen_before != gen_after {
            anyhow::bail!(
                "circuit cache was evicted during verification; result discarded to avoid attesting against a stale bundle"
            );
        }

        if !verified.valid {
            anyhow::bail!("proof verification failed");
        }

        Ok(VerifyResult {
            rescaled_outputs: verified.outputs,
            scale_base: verified.scale_base,
            scale_exponent: verified.scale_exponent,
        })
    })
    .await
    .context("verification task panicked")?
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
