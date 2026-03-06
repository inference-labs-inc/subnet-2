use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use btlightning::{LightningClient, LightningError, QuicAxonInfo, QuicRequest, Signer};
use sha2::{Digest, Sha256};
use sn2_chain::Wallet;
use tracing::{info, warn};

struct WalletSigner(Arc<Wallet>);

impl Signer for WalletSigner {
    fn sign(&self, message: &[u8]) -> btlightning::Result<Vec<u8>> {
        self.0
            .sign_hotkey(message)
            .map_err(|e| btlightning::LightningError::Signing(e.to_string()))
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum TransportPreference {
    Unknown,
    Quic,
    HttpOnly,
}

pub struct MinerQueryClient {
    lightning: LightningClient,
    http: reqwest::Client,
    wallet: Option<Arc<Wallet>>,
    transport_cache: Mutex<HashMap<String, (TransportPreference, String)>>,
}

impl MinerQueryClient {
    pub fn new(wallet: Arc<Wallet>) -> Result<Self> {
        let mut lightning = LightningClient::new(wallet.hotkey_ss58().to_string());
        lightning.set_signer(Box::new(WalletSigner(wallet.clone())));
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(64)
            .tcp_nodelay(true)
            .build()
            .context("creating HTTP client")?;

        Ok(Self {
            lightning,
            http,
            wallet: Some(wallet),
            transport_cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn new_unsigned() -> Result<Self> {
        let lightning = LightningClient::new("loopback".to_string());
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(64)
            .tcp_nodelay(true)
            .build()
            .context("creating HTTP client")?;
        Ok(Self {
            lightning,
            http,
            wallet: None,
            transport_cache: Mutex::new(HashMap::new()),
        })
    }

    pub async fn init_quic(&mut self) -> Result<()> {
        self.lightning
            .initialize_connections(vec![])
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("initializing QUIC endpoint")?;
        info!("QUIC endpoint initialized");
        Ok(())
    }

    pub fn build_signing_headers(
        &self,
        body: &serde_json::Value,
        miner_hotkey: &str,
    ) -> Result<HashMap<String, String>> {
        let wallet = match &self.wallet {
            Some(w) => w,
            None => return Ok(HashMap::new()),
        };

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_string();

        let body_str = serde_json::to_string(body)?;
        let body_hash = hex::encode(Sha256::digest(body_str.as_bytes()));
        let message = sn2_types::signing_message(&nonce, wallet.hotkey_ss58(), &body_hash);
        let sig_bytes = wallet.sign_hotkey(message.as_bytes())?;
        let sig_hex = format!("0x{}", hex::encode(&sig_bytes));

        let mut headers = HashMap::new();
        headers.insert("nonce".to_string(), nonce);
        headers.insert("signature".to_string(), sig_hex);
        headers.insert(
            "validator-hotkey".to_string(),
            wallet.hotkey_ss58().to_string(),
        );
        headers.insert("miner-hotkey".to_string(), miner_hotkey.to_string());
        Ok(headers)
    }

    pub fn lightning_mut(&mut self) -> &mut LightningClient {
        &mut self.lightning
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

        let resp_body: serde_json::Value = response
            .deserialize_data()
            .map_err(anyhow::Error::from)
            .context("deserializing QUIC response")?;
        Ok((resp_body, elapsed))
    }

    pub async fn query_miner_http(
        &self,
        ip: &str,
        port: u16,
        synapse_type: &str,
        body: &serde_json::Value,
        headers: &HashMap<String, String>,
        timeout_secs: f64,
    ) -> Result<(serde_json::Value, f64)> {
        let url = sn2_types::format_http_url(ip, port, synapse_type);

        let mut req = self
            .http
            .post(&url)
            .timeout(std::time::Duration::from_secs_f64(timeout_secs))
            .json(body);

        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }

        let start = Instant::now();
        let response = req.send().await.context("HTTP query to miner")?;
        let elapsed = start.elapsed().as_secs_f64();

        if !response.status().is_success() {
            let status = response.status();
            let body_text = response.text().await.unwrap_or_default();
            let truncated = match body_text.char_indices().nth(500) {
                Some((idx, _)) => &body_text[..idx],
                None => &body_text,
            };
            anyhow::bail!("HTTP {status} from miner: {truncated}");
        }

        let body: serde_json::Value = response.json().await.context("parsing miner response")?;
        Ok((body, elapsed))
    }

    pub async fn query_miner_adaptive(
        &self,
        ip: &str,
        port: u16,
        hotkey: &str,
        synapse_type: &str,
        body: &serde_json::Value,
        timeout_secs: f64,
    ) -> Result<(serde_json::Value, f64)> {
        let addr = format!("{ip}:{port}");
        let pref = self.get_transport(hotkey);

        if pref != TransportPreference::Quic {
            let headers = self.build_signing_headers(body, hotkey)?;
            return self
                .query_miner_http(ip, port, synapse_type, body, &headers, timeout_secs)
                .await;
        }

        let axon = QuicAxonInfo::new(hotkey.to_string(), ip.to_string(), port, 4);
        let data: HashMap<String, serde_json::Value> = match serde_json::from_value(body.clone()) {
            Ok(d) => d,
            Err(e) => {
                return Err(anyhow::Error::from(e).context("deserializing QUIC payload"));
            }
        };

        match self
            .query_miner_quic(&axon, synapse_type, data, timeout_secs)
            .await
        {
            Ok(result) => Ok(result),
            Err(e) if is_connection_error(&e) => {
                self.set_transport(hotkey, TransportPreference::HttpOnly, &addr);
                warn!(
                    hotkey = hotkey,
                    error = ?e,
                    "QUIC connection failed, falling back to HTTP"
                );
                let headers = self.build_signing_headers(body, hotkey)?;
                self.query_miner_http(ip, port, synapse_type, body, &headers, timeout_secs)
                    .await
            }
            Err(e) => Err(e),
        }
    }

    fn get_transport(&self, hotkey: &str) -> TransportPreference {
        let cache = self
            .transport_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        cache
            .get(hotkey)
            .map(|(pref, _)| *pref)
            .unwrap_or(TransportPreference::Unknown)
    }

    fn set_transport(&self, hotkey: &str, pref: TransportPreference, addr: &str) {
        self.transport_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(hotkey.to_string(), (pref, addr.to_string()));
    }

    pub fn clear_transport_cache(&self) {
        self.transport_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    pub async fn seed_transport_cache(&self, miners: &[QuicAxonInfo]) {
        let stats = match self.lightning.get_connection_stats().await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to read connection stats, preserving existing cache");
                return;
            }
        };

        let mut cache = self
            .transport_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        cache.clear();

        let mut quic_count = 0u32;

        for miner in miners {
            let key = format!("connection_{}", miner.addr_key());
            if stats.get(&key).map(|s| s == "active").unwrap_or(false) {
                let addr = miner.addr_key().to_string();
                cache.insert(miner.hotkey.clone(), (TransportPreference::Quic, addr));
                quic_count += 1;
            }
        }

        info!(
            quic = quic_count,
            http = miners.len() as u32 - quic_count,
            "seeded transport cache from connection stats"
        );
    }
}

fn is_connection_error(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(le) = cause.downcast_ref::<LightningError>() {
            return matches!(
                le,
                LightningError::Connection(_)
                    | LightningError::Transport(_)
                    | LightningError::Handshake(_)
                    | LightningError::Stream(_)
            );
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_client() -> MinerQueryClient {
        MinerQueryClient {
            lightning: LightningClient::new("test".to_string()),
            http: reqwest::Client::new(),
            wallet: None,
            transport_cache: Mutex::new(HashMap::new()),
        }
    }

    #[test]
    fn unknown_returned_for_new_hotkey() {
        let client = make_client();
        let pref = client.get_transport("hk1");
        assert_eq!(pref, TransportPreference::Unknown);
    }

    #[test]
    fn set_transport_overrides_preference() {
        let client = make_client();
        client.set_transport("hk1", TransportPreference::Quic, "1.2.3.4:8091");
        let pref = client.get_transport("hk1");
        assert_eq!(pref, TransportPreference::Quic);
    }

    #[test]
    fn clear_transport_cache_resets_all() {
        let client = make_client();
        client.set_transport("hk1", TransportPreference::Quic, "1.2.3.4:8091");
        client.set_transport("hk2", TransportPreference::HttpOnly, "5.6.7.8:8091");
        client.clear_transport_cache();
        let pref = client.get_transport("hk1");
        assert_eq!(pref, TransportPreference::Unknown);
    }

    #[test]
    fn is_connection_error_classifies_transport_errors() {
        let conn = anyhow::Error::from(LightningError::Connection("reset".into()));
        assert!(is_connection_error(&conn));

        let transport = anyhow::Error::from(LightningError::Transport("timeout".into()));
        assert!(is_connection_error(&transport));

        let handshake = anyhow::Error::from(LightningError::Handshake("mismatch".into()));
        assert!(is_connection_error(&handshake));

        let stream = anyhow::Error::from(LightningError::Stream("closed".into()));
        assert!(is_connection_error(&stream));
    }

    #[test]
    fn is_connection_error_rejects_non_transport_errors() {
        let other = anyhow::anyhow!("some random error");
        assert!(!is_connection_error(&other));

        let serialization = anyhow::Error::from(LightningError::Serialization("bad format".into()));
        assert!(!is_connection_error(&serialization));
    }

    #[test]
    fn independent_hotkeys_have_independent_state() {
        let client = make_client();
        client.set_transport("hk1", TransportPreference::Quic, "1.2.3.4:8091");
        client.set_transport("hk2", TransportPreference::HttpOnly, "5.6.7.8:8091");
        assert_eq!(client.get_transport("hk1"), TransportPreference::Quic);
        assert_eq!(client.get_transport("hk2"), TransportPreference::HttpOnly);
        assert_eq!(client.get_transport("hk3"), TransportPreference::Unknown);
    }
}
