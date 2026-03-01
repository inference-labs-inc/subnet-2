use anyhow::{bail, Context, Result};
use sp_core::crypto::Ss58Codec;
use sp_core::sr25519;
use sp_core::Pair;

use sn2_types::MAX_SIGNATURE_LIFESPAN;

pub fn verify_request_signature(
    nonce: &str,
    validator_hotkey: &str,
    payload_hash: &str,
    signature_hex: &str,
) -> Result<bool> {
    let message = sn2_types::signing_message(nonce, validator_hotkey, payload_hash);
    let message_bytes = message.as_bytes();

    let sig_hex = signature_hex
        .strip_prefix("0x")
        .or_else(|| signature_hex.strip_prefix("0X"))
        .unwrap_or(signature_hex);
    let sig_bytes = hex::decode(sig_hex).context("decoding signature hex")?;
    if sig_bytes.len() != 64 {
        bail!("signature must be 64 bytes, got {}", sig_bytes.len());
    }
    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(&sig_bytes);
    let sig = sr25519::Signature::from_raw(sig_arr);
    let pubkey = sr25519::Public::from_ss58check(validator_hotkey)
        .map_err(|e| anyhow::anyhow!("invalid SS58 hotkey address: {e:?}"))?;

    if !sr25519::Pair::verify(&sig, message_bytes, &pubkey) {
        return Ok(false);
    }

    let nonce_ts: u128 = nonce.parse().context("nonce is not a valid timestamp")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before UNIX epoch: {e}"))?
        .as_nanos();

    const ALLOWED_SKEW_NANOS: u128 = 30 * 1_000_000_000;
    if nonce_ts > now.saturating_add(ALLOWED_SKEW_NANOS) {
        return Ok(false);
    }

    let lifespan_nanos = (MAX_SIGNATURE_LIFESPAN as u128) * 1_000_000_000;
    if now.saturating_sub(nonce_ts) > lifespan_nanos {
        return Ok(false);
    }

    Ok(true)
}
