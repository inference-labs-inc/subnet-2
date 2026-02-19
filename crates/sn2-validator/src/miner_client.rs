use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use btlightning::{LightningClient, QuicAxonInfo, QuicRequest};
use sha2::{Digest, Sha256};
use sp_core::{sr25519, Pair};

pub struct MinerQueryClient {
    lightning: LightningClient,
    http: reqwest::Client,
    hotkey_pair: sr25519::Pair,
    hotkey_ss58: String,
}

impl MinerQueryClient {
    pub fn new(hotkey_pair: sr25519::Pair, hotkey_ss58: &str) -> Result<Self> {
        let lightning = LightningClient::new(hotkey_ss58.to_string());
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(64)
            .tcp_nodelay(true)
            .build()
            .context("creating HTTP client")?;

        Ok(Self {
            lightning,
            http,
            hotkey_pair,
            hotkey_ss58: hotkey_ss58.to_string(),
        })
    }

    pub fn build_signing_headers(
        &self,
        body: &serde_json::Value,
        miner_hotkey: &str,
    ) -> HashMap<String, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_string();

        let body_str = serde_json::to_string(body).unwrap_or_default();
        let body_hash = hex::encode(Sha256::digest(body_str.as_bytes()));
        let message = format!("{}:{}:{}", nonce, self.hotkey_ss58, body_hash);
        let signature = self.hotkey_pair.sign(message.as_bytes());
        let sig_hex = format!("0x{}", hex::encode(signature.0));

        let mut headers = HashMap::new();
        headers.insert("nonce".to_string(), nonce);
        headers.insert("signature".to_string(), sig_hex);
        headers.insert("validator-hotkey".to_string(), self.hotkey_ss58.clone());
        headers.insert("miner-hotkey".to_string(), miner_hotkey.to_string());
        headers
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
        let request = QuicRequest {
            synapse_type: synapse_type.to_string(),
            data,
        };

        let start = Instant::now();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs_f64(timeout_secs),
            self.lightning.query_axon(axon.clone(), request),
        )
        .await
        .context("QUIC query timed out")?
        .context("QUIC query failed")?;
        let elapsed = start.elapsed().as_secs_f64();

        if !response.success {
            anyhow::bail!("QUIC query failed");
        }

        let body = serde_json::to_value(&response.data)?;
        Ok((body, elapsed))
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
        let url = format!("http://{}:{}/{}", ip, port, synapse_type);

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
}
