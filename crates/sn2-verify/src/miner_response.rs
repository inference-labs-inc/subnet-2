use anyhow::{Context, Result};
use serde_json::Value;

pub struct DSliceFields {
    pub proof_hex: String,
    pub witness_hex: Option<String>,
    pub is_incremental: bool,
    pub proof_size: usize,
}

pub fn extract_dslice_fields(body: &Value) -> Result<DSliceFields> {
    let has_proof = body.get("proof").and_then(Value::as_str).is_some();
    let explicit_failure = body
        .get("success")
        .and_then(Value::as_bool)
        .map(|v| !v)
        .unwrap_or(false);

    if explicit_failure || !has_proof {
        let error = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown miner error");
        anyhow::bail!("miner reported failure: {error}");
    }

    let proof_hex = body
        .get("proof")
        .and_then(Value::as_str)
        .context("missing 'proof' field in miner response")?
        .to_string();

    let witness_hex = body
        .get("witness")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from);

    let is_incremental = witness_hex.is_some();
    let proof_size = proof_hex.len();

    Ok(DSliceFields {
        proof_hex,
        witness_hex,
        is_incremental,
        proof_size,
    })
}
