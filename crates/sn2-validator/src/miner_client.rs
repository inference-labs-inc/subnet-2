use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use btlightning::{LightningClient, QuicAxonInfo, QuicRequest, Signer};
use rmpv::Value as RmpvValue;
use serde_json::Value as JsonValue;
use sn2_chain::Wallet;
use sn2_types::tensor_codec::MSGPACK_MAX_DEPTH;
use tracing::{debug, info};

/// Converts an `rmpv::Value` tree into a `serde_json::Value`, hex-encoding any
/// MessagePack `bin` payloads so they appear as hex strings to the rest of the
/// validator pipeline. This is the wire-boundary adapter that lets miners emit
/// binary proof/witness/public_signals without forcing changes through
/// `MinerResponse`, the verifier, the proof uploader, and the relay.
fn rmpv_to_json_value(value: RmpvValue) -> Result<JsonValue> {
    rmpv_to_json_value_bounded(value, 0)
}

fn rmpv_to_json_value_bounded(value: RmpvValue, depth: usize) -> Result<JsonValue> {
    anyhow::ensure!(
        depth <= MSGPACK_MAX_DEPTH,
        "miner response nesting depth exceeds {MSGPACK_MAX_DEPTH}"
    );
    Ok(match value {
        RmpvValue::Nil => JsonValue::Null,
        RmpvValue::Boolean(b) => JsonValue::Bool(b),
        RmpvValue::Integer(i) => {
            if let Some(u) = i.as_u64() {
                JsonValue::Number(u.into())
            } else if let Some(s) = i.as_i64() {
                JsonValue::Number(s.into())
            } else if let Some(f) = i.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(JsonValue::Number)
                    .unwrap_or(JsonValue::Null)
            } else {
                JsonValue::Null
            }
        }
        RmpvValue::F32(f) => serde_json::Number::from_f64(f as f64)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        RmpvValue::F64(f) => serde_json::Number::from_f64(f)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        RmpvValue::String(s) => s
            .into_str()
            .map(JsonValue::String)
            .unwrap_or(JsonValue::Null),
        RmpvValue::Binary(bytes) => JsonValue::String(hex::encode(bytes)),
        RmpvValue::Array(items) => JsonValue::Array(
            items
                .into_iter()
                .map(|item| rmpv_to_json_value_bounded(item, depth + 1))
                .collect::<Result<Vec<_>>>()?,
        ),
        RmpvValue::Map(pairs) => {
            let mut obj = serde_json::Map::with_capacity(pairs.len());
            for (k, v) in pairs {
                let key = match k {
                    // MessagePack `str` is byte-clean and not guaranteed UTF-8.
                    // Fall back to a hex-prefixed encoding for non-UTF-8 bytes
                    // so each malformed key stays distinct rather than
                    // collapsing onto an empty string.
                    RmpvValue::String(s) => {
                        if s.is_str() {
                            s.into_str().unwrap_or_default()
                        } else {
                            format!("__msgpack_str_hex:{}", hex::encode(s.into_bytes()))
                        }
                    }
                    RmpvValue::Integer(i) => i.to_string(),
                    other => format!("{other}"),
                };
                obj.insert(key, rmpv_to_json_value_bounded(v, depth + 1)?);
            }
            JsonValue::Object(obj)
        }
        RmpvValue::Ext(_, _) => JsonValue::Null,
    })
}

struct WalletSigner(Arc<Wallet>);

impl Signer for WalletSigner {
    fn sign(&self, message: &[u8]) -> btlightning::Result<Vec<u8>> {
        self.0
            .sign_hotkey(message)
            .map_err(|e| btlightning::LightningError::Signing(e.to_string()))
    }
}

pub struct MinerQueryClient {
    lightning: LightningClient,
}

impl MinerQueryClient {
    pub fn new(wallet: Arc<Wallet>) -> Result<Self> {
        let config = btlightning::LightningClientConfig {
            max_frame_payload_bytes: sn2_types::TRANSPORT_PAYLOAD_LIMIT,
            max_stream_payload_bytes: sn2_types::TRANSPORT_PAYLOAD_LIMIT,
            ..Default::default()
        };
        let mut lightning = LightningClient::with_config(wallet.hotkey_ss58().to_string(), config)
            .context("building lightning client")?;
        lightning.set_signer(Box::new(WalletSigner(wallet.clone())));

        Ok(Self { lightning })
    }

    pub async fn init_quic(&mut self, initial_miners: Vec<QuicAxonInfo>) -> Result<()> {
        self.lightning
            .initialize_connections(initial_miners)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("initializing QUIC endpoint")?;
        info!("QUIC endpoint initialized");
        Ok(())
    }

    pub fn lightning_mut(&mut self) -> &mut LightningClient {
        &mut self.lightning
    }

    pub async fn query_miner(
        &self,
        ip: &str,
        port: u16,
        hotkey: &str,
        synapse_type: &str,
        body: &serde_json::Value,
        timeout_secs: f64,
    ) -> Result<(serde_json::Value, f64)> {
        let axon = QuicAxonInfo::new(hotkey.to_string(), ip.to_string(), port, 4);
        let data: HashMap<String, serde_json::Value> =
            serde_json::from_value(body.clone()).context("deserializing QUIC payload")?;
        let (resp, elapsed) = self
            .query_miner_quic(&axon, synapse_type, data, timeout_secs)
            .await?;
        debug!(
            addr = %format!("{ip}:{port}"),
            transport = "quic",
            synapse = synapse_type,
            elapsed,
            "miner query completed"
        );
        Ok((resp, elapsed))
    }

    pub async fn query_miner_quic(
        &self,
        axon: &QuicAxonInfo,
        synapse_type: &str,
        data: HashMap<String, serde_json::Value>,
        timeout_secs: f64,
    ) -> Result<(serde_json::Value, f64)> {
        let request = QuicRequest::from_typed(synapse_type, &data)
            .map_err(anyhow::Error::from)
            .context("serializing QUIC request")?;

        let start = Instant::now();
        let response = self
            .lightning
            .query_axon_with_timeout(axon.clone(), request, Duration::from_secs_f64(timeout_secs))
            .await
            .map_err(anyhow::Error::from)
            .context("QUIC query")?;
        let elapsed = start.elapsed().as_secs_f64();

        let response = response.into_result().map_err(anyhow::Error::from)?;

        let mut obj = serde_json::Map::with_capacity(response.data.len());
        for (k, v) in response.data {
            obj.insert(
                k,
                rmpv_to_json_value(v).context("decoding miner response value")?,
            );
        }
        let resp_body = serde_json::Value::Object(obj);
        Ok((resp_body, elapsed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_field_becomes_hex_string() {
        let bytes = vec![0xde, 0xad, 0xbe, 0xef];
        let value = RmpvValue::Map(vec![(
            RmpvValue::String("proof".into()),
            RmpvValue::Binary(bytes.clone()),
        )]);
        let json = rmpv_to_json_value(value).unwrap();
        assert_eq!(json["proof"], JsonValue::String(hex::encode(&bytes)));
    }

    #[test]
    fn legacy_string_payloads_pass_through() {
        let value = RmpvValue::Map(vec![(
            RmpvValue::String("proof".into()),
            RmpvValue::String("deadbeef".into()),
        )]);
        let json = rmpv_to_json_value(value).unwrap();
        assert_eq!(json["proof"], JsonValue::String("deadbeef".into()));
    }

    #[test]
    fn non_utf8_map_keys_are_lossless() {
        // Hand-encoded MessagePack: fixmap(2) { fixstr[2]=0xff,0x00 => 1, fixstr[2]=0xfe,0x01 => 2 }
        let bytes: &[u8] = &[
            0x82, // fixmap, 2 entries
            0xa2, 0xff, 0x00, // fixstr(2) with non-utf8 bytes
            0x01, 0xa2, 0xfe, 0x01, // fixstr(2) with different non-utf8 bytes
            0x02,
        ];
        let value = rmpv::decode::read_value(&mut std::io::Cursor::new(bytes)).expect("decode");
        let json = rmpv_to_json_value(value).unwrap();
        let obj = json.as_object().expect("map");
        assert_eq!(obj.len(), 2, "non-utf8 keys must not collide");
        assert!(obj.keys().all(|k| k.starts_with("__msgpack_str_hex:")));
        assert!(obj.contains_key("__msgpack_str_hex:ff00"));
        assert!(obj.contains_key("__msgpack_str_hex:fe01"));
    }

    #[test]
    fn nested_binary_in_arrays_is_hex_encoded() {
        let value = RmpvValue::Array(vec![
            RmpvValue::Binary(vec![0x01, 0x02]),
            RmpvValue::String("abc".into()),
        ]);
        let json = rmpv_to_json_value(value).unwrap();
        assert_eq!(json[0], JsonValue::String("0102".into()));
        assert_eq!(json[1], JsonValue::String("abc".into()));
    }

    #[test]
    fn nesting_beyond_depth_limit_is_rejected() {
        let mut value = RmpvValue::Nil;
        for _ in 0..=MSGPACK_MAX_DEPTH {
            value = RmpvValue::Array(vec![value]);
        }
        let err = rmpv_to_json_value(value).unwrap_err().to_string();
        assert!(err.contains("nesting depth"), "unexpected error: {err}");
    }

    #[test]
    fn nesting_at_depth_limit_is_accepted() {
        let mut value = RmpvValue::Boolean(true);
        for _ in 0..MSGPACK_MAX_DEPTH {
            value = RmpvValue::Array(vec![value]);
        }
        assert!(rmpv_to_json_value(value).is_ok());
    }
}
