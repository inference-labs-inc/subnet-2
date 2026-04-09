use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use anyhow::{Context, Result};
use tracing::{info, warn};

use jstprove_circuits::onnx::{
    deserialize_circuit_bn254, deserialize_circuit_goldilocks_whir_pq, flatten_circuit_bn254,
    flatten_circuit_goldilocks_whir_pq, verify_and_extract_bn254_with_flat_ref,
    verify_and_extract_goldilocks_whir_pq_with_flat_ref, FlatCircuitBN254,
    FlatCircuitGoldilocksWhirPQ,
};

enum CachedCircuit {
    BN254(Arc<FlatCircuitBN254>),
    GoldilocksWhirPQ(Arc<FlatCircuitGoldilocksWhirPQ>),
}

static CIRCUIT_CACHE: LazyLock<RwLock<HashMap<String, CachedCircuit>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

static LOADING_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static EVICTION_GEN: AtomicU64 = AtomicU64::new(0);

use crate::protocol::{StoreResponse, VerifyAndStoreRequest, VerifyRequest, VerifyResponse};
use crate::store::{StoredTile, TileStore};

pub fn evict_circuit_cache(path_prefix: &str) {
    let mut cache = CIRCUIT_CACHE.write().unwrap();
    EVICTION_GEN.fetch_add(1, Ordering::SeqCst);
    let before = cache.len();
    cache.retain(|k, _| !k.starts_with(path_prefix));
    let evicted = before - cache.len();
    if evicted > 0 {
        info!(prefix = %path_prefix, evicted, remaining = cache.len(), "evicted circuit cache entries");
    }
}

pub fn clear_circuit_cache() {
    let mut cache = CIRCUIT_CACHE.write().unwrap();
    EVICTION_GEN.fetch_add(1, Ordering::SeqCst);
    let count = cache.len();
    cache.clear();
    if count > 0 {
        info!(cleared = count, "cleared circuit cache");
    }
}

fn get_or_load(circuit_path: &str) -> Result<()> {
    {
        let cache = CIRCUIT_CACHE.read().unwrap();
        if cache.contains_key(circuit_path) {
            return Ok(());
        }
    }

    let path_lock = {
        let mut loading = LOADING_LOCKS.lock().unwrap();
        Arc::clone(loading.entry(circuit_path.to_string()).or_default())
    };
    let _guard = path_lock.lock().unwrap();

    {
        let cache = CIRCUIT_CACHE.read().unwrap();
        if cache.contains_key(circuit_path) {
            return Ok(());
        }
    }

    let gen_before = EVICTION_GEN.load(Ordering::SeqCst);

    let circuit_bytes = {
        let p = std::path::Path::new(circuit_path);
        if p.is_dir() {
            jstprove_io::bundle::read_circuit_blob(p)
                .with_context(|| format!("reading circuit from bundle {circuit_path}"))?
        } else {
            std::fs::read(circuit_path).with_context(|| format!("reading {circuit_path}"))?
        }
    };

    let cached = if let Ok(layered) = deserialize_circuit_bn254(&circuit_bytes) {
        let flat = flatten_circuit_bn254(&layered);
        drop(layered);
        info!(
            path = %circuit_path,
            config = "BN254",
            size_mb = circuit_bytes.len() as f64 / (1024.0 * 1024.0),
            "cached flattened circuit"
        );
        CachedCircuit::BN254(Arc::new(flat))
    } else if let Ok(layered) = deserialize_circuit_goldilocks_whir_pq(&circuit_bytes) {
        let flat = flatten_circuit_goldilocks_whir_pq(&layered);
        drop(layered);
        info!(
            path = %circuit_path,
            config = "GoldilocksWhirPQ",
            size_mb = circuit_bytes.len() as f64 / (1024.0 * 1024.0),
            "cached flattened circuit"
        );
        CachedCircuit::GoldilocksWhirPQ(Arc::new(flat))
    } else {
        anyhow::bail!(
            "circuit {circuit_path} does not match any supported config (BN254, GoldilocksWhirPQ)"
        );
    };

    let mut cache = CIRCUIT_CACHE.write().unwrap();
    if EVICTION_GEN.load(Ordering::SeqCst) == gen_before {
        cache.insert(circuit_path.to_string(), cached);
    } else {
        info!(
            path = %circuit_path,
            "eviction occurred during load, skipping cache insert"
        );
    }

    Ok(())
}

pub struct VerifyResult {
    pub rescaled_outputs: Vec<f64>,
    pub scale_base: u64,
    pub scale_exponent: u64,
}

fn verify_with_cache(
    circuit_path: &str,
    witness_bytes: &[u8],
    proof_bytes: &[u8],
    num_inputs: usize,
    expected_inputs: Option<&[f64]>,
) -> Result<VerifyResult> {
    get_or_load(circuit_path)?;

    let cache = CIRCUIT_CACHE.read().unwrap();
    let cached = cache
        .get(circuit_path)
        .ok_or_else(|| anyhow::anyhow!("circuit evicted during verify"))?;

    let result = match cached {
        CachedCircuit::BN254(circuit) => verify_and_extract_bn254_with_flat_ref(
            circuit,
            witness_bytes,
            proof_bytes,
            num_inputs,
            expected_inputs,
        ),
        CachedCircuit::GoldilocksWhirPQ(circuit) => {
            verify_and_extract_goldilocks_whir_pq_with_flat_ref(
                circuit,
                witness_bytes,
                proof_bytes,
                num_inputs,
                expected_inputs,
            )
        }
    }
    .map_err(|e| anyhow::anyhow!("verification: {e}"))?;

    if !result.valid {
        anyhow::bail!("proof verification failed");
    }

    Ok(VerifyResult {
        rescaled_outputs: result.outputs,
        scale_base: result.scale_base,
        scale_exponent: result.scale_exponent,
    })
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

    tokio::task::spawn_blocking(move || {
        let witness_bytes = hex::decode(witness_hex.trim()).context("hex-decoding witness")?;
        let proof_bytes = hex::decode(proof_hex.trim()).context("hex-decoding proof")?;

        verify_with_cache(
            &circuit_path,
            &witness_bytes,
            &proof_bytes,
            num_inputs,
            expected_inputs.as_deref(),
        )
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
