use std::sync::Arc;

use anyhow::{Context, Result};
use std::path::PathBuf;
use tempfile::TempDir;
use tracing::warn;

use crate::expander;
use crate::field;
use crate::protocol::{StoreResponse, VerifyAndStoreRequest, VerifyRequest, VerifyResponse};
use crate::store::{StoredTile, TileStore};
use crate::witness;

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
    pcs_type: &str,
) -> Result<VerifyResult> {
    let witness_hex = witness_hex.to_string();
    let circuit_path = PathBuf::from(circuit_path);
    let proof_hex = proof_hex.to_string();
    let expected_inputs = expected_inputs.clone();
    let pcs_type = pcs_type.to_string();

    let (_witness_data, extracted_io, tmp_dir) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            let witness_bytes = hex::decode(witness_hex.trim()).context("hex-decoding witness")?;
            let proof_bytes = hex::decode(proof_hex.trim()).context("hex-decoding proof")?;

            let witness_raw =
                witness::decompress_if_needed(&witness_bytes).context("decompressing witness")?;

            let tmp_dir = TempDir::new_in(std::env::temp_dir()).context("creating temp dir")?;
            let witness_path = tmp_dir.path().join("witness.bin");
            let proof_path = tmp_dir.path().join("proof.bin");
            std::fs::write(&witness_path, &witness_raw).context("writing witness")?;
            std::fs::write(&proof_path, &proof_bytes).context("writing proof")?;

            let wd = witness::load_witness_from_bytes(&witness_bytes)
                .context("parsing witness binary")?;
            let extracted =
                witness::extract_io(&wd, num_inputs).context("extracting IO from witness")?;

            if let Some(ref expected) = expected_inputs {
                if expected.len() != extracted.inputs.len() {
                    anyhow::bail!(
                        "input length mismatch: expected {}, witness has {}",
                        expected.len(),
                        extracted.inputs.len()
                    );
                }
                let scaled = field::scale_to_field(
                    expected,
                    extracted.scale_base,
                    extracted.scale_exponent,
                    &extracted.modulus,
                );
                if !field::compare_field_values(&scaled, &extracted.inputs, &extracted.modulus, 1) {
                    anyhow::bail!("input verification failed: witness inputs don't match expected");
                }
            }

            Ok((wd, extracted, tmp_dir))
        })
        .await
        .context("blocking task panicked")?
        .context("verification preprocessing")?;

    let witness_path = tmp_dir.path().join("witness.bin");
    let proof_path = tmp_dir.path().join("proof.bin");

    let success =
        expander::run_expander_verify(&circuit_path, &witness_path, &proof_path, &pcs_type)
            .await
            .context("running expander-exec")?;

    drop(tmp_dir);

    if !success {
        anyhow::bail!("expander-exec verification failed");
    }

    Ok(VerifyResult {
        rescaled_outputs: extracted_io.rescaled_outputs,
        scale_base: extracted_io.scale_base,
        scale_exponent: extracted_io.scale_exponent,
    })
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
            let expected_len = channels * height * width;
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
            store.insert(
                req.store_key,
                StoredTile {
                    data: result.rescaled_outputs,
                    channels,
                    height,
                    width,
                },
            );
            StoreResponse::ok(req.request_id)
        }
        Err(e) => {
            warn!(request_id = %req.request_id, error = %e, "verify_and_store failed");
            StoreResponse::error(req.request_id, format!("{e:#}"))
        }
    }
}
