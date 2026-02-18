use anyhow::{Context, Result};
use sp_core::sr25519;
use sp_core::Pair;

use sn2_types::MAX_SIGNATURE_LIFESPAN;

pub fn verify_request_signature(
    nonce: &str,
    validator_hotkey: &str,
    payload_hash: &str,
    signature_hex: &str,
) -> Result<bool> {
    let message = format!("{nonce}:{validator_hotkey}:{payload_hash}");
    let message_bytes = message.as_bytes();

    let sig_bytes = hex::decode(signature_hex).context("decoding signature hex")?;
    if sig_bytes.len() != 64 {
        anyhow::bail!("signature must be 64 bytes, got {}", sig_bytes.len());
    }

    let sig = sr25519::Signature::from_raw(sig_bytes.try_into().unwrap());
    let pubkey = sr25519::Public::from_raw(
        hex::decode(validator_hotkey.trim_start_matches("0x"))
            .context("decoding hotkey hex")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("hotkey must be 32 bytes"))?,
    );

    if !sr25519::Pair::verify(&sig, message_bytes, &pubkey) {
        return Ok(false);
    }

    let nonce_ts: u64 = nonce.parse().unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if now.saturating_sub(nonce_ts) > MAX_SIGNATURE_LIFESPAN {
        return Ok(false);
    }

    Ok(true)
}
