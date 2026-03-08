use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use btlightning::{LightningClient, QuicAxonInfo, QuicRequest, Signer};
use sha2::{Digest, Sha256};
use sn2_chain::Wallet;
use tracing::info;

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
    http: reqwest::Client,
    wallet: Option<Arc<Wallet>>,
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
        info!(
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
        let elapsed = start.elapsed().as_secs_f64();
        Ok((body, elapsed))
    }
}
