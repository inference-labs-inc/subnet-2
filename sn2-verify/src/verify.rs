use anyhow::{Context, Result};
use std::path::PathBuf;
use tempfile::TempDir;
use tracing::warn;

use crate::expander;
use crate::field;
use crate::protocol::{VerifyRequest, VerifyResponse};
use crate::witness;

pub async fn handle_request(req: VerifyRequest) -> VerifyResponse {
    match handle_inner(&req).await {
        Ok(resp) => resp,
        Err(e) => {
            warn!(request_id = %req.request_id, error = %e, "verification failed");
            VerifyResponse::error(req.request_id.clone(), format!("{e:#}"))
        }
    }
}

async fn handle_inner(req: &VerifyRequest) -> Result<VerifyResponse> {
    let request_id = req.request_id.clone();
    let shm_path = PathBuf::from(&req.witness_shm_path);
    let circuit_path = PathBuf::from(&req.circuit_path);
    let proof_hex = req.proof_hex.clone();
    let num_inputs = req.num_inputs;
    let expected_inputs = req.expected_inputs.clone();
    let pcs_type = req.pcs_type.clone();

    let (_witness_data, extracted_io, tmp_dir) = tokio::task::spawn_blocking(move || -> Result<_> {
        let witness_hex = std::fs::read_to_string(&shm_path)
            .with_context(|| format!("reading witness hex from {}", shm_path.display()))?;
        let _ = std::fs::remove_file(&shm_path);

        let witness_bytes = hex::decode(witness_hex.trim())
            .context("hex-decoding witness")?;
        let proof_bytes = hex::decode(proof_hex.trim())
            .context("hex-decoding proof")?;

        let tmp_dir = TempDir::new_in(std::env::temp_dir())
            .context("creating temp dir")?;
        let witness_path = tmp_dir.path().join("witness.bin");
        let proof_path = tmp_dir.path().join("proof.bin");
        std::fs::write(&witness_path, &witness_bytes).context("writing witness")?;
        std::fs::write(&proof_path, &proof_bytes).context("writing proof")?;

        let wd = witness::load_witness_from_bytes(&witness_bytes)
            .context("parsing witness binary")?;
        let extracted = witness::extract_io(&wd, num_inputs)
            .context("extracting IO from witness")?;

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

    let success = expander::run_expander_verify(
        &circuit_path,
        &witness_path,
        &proof_path,
        &pcs_type,
    )
    .await
    .context("running expander-exec")?;

    drop(tmp_dir);

    if !success {
        return Ok(VerifyResponse::error(
            request_id,
            "expander-exec verification failed".into(),
        ));
    }

    Ok(VerifyResponse::ok(
        request_id,
        extracted_io.rescaled_outputs,
        extracted_io.scale_base,
        extracted_io.scale_exponent,
    ))
}
