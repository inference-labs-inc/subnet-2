use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use btlightning::{LightningClient, QuicAxonInfo, QuicRequest};
use sha2::{Digest, Sha256};
use sn2_chain::Wallet;

pub struct MinerQueryClient {
    lightning: LightningClient,
    http: reqwest::Client,
    wallet: Arc<Wallet>,
}

impl MinerQueryClient {
    pub fn new(wallet: Arc<Wallet>) -> Result<Self> {
        let lightning = LightningClient::new(wallet.hotkey_ss58().to_string());
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(64)
            .tcp_nodelay(true)
            .build()
            .context("creating HTTP client")?;

        Ok(Self {
            lightning,
            http,
            wallet,
        })
    }

    pub fn build_signing_headers(
        &self,
        body: &serde_json::Value,
        miner_hotkey: &str,
    ) -> Result<HashMap<String, String>> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_string();

        let body_str = serde_json::to_string(body)?;
        let body_hash = hex::encode(Sha256::digest(body_str.as_bytes()));
        let message = format!("{}:{}:{}", nonce, self.wallet.hotkey_ss58(), body_hash);
        let sig_bytes = self.wallet.sign_hotkey(message.as_bytes())?;
        let sig_hex = format!("0x{}", hex::encode(&sig_bytes));

        let mut headers = HashMap::new();
        headers.insert("nonce".to_string(), nonce);
        headers.insert("signature".to_string(), sig_hex);
        headers.insert(
            "validator-hotkey".to_string(),
            self.wallet.hotkey_ss58().to_string(),
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
        let rmpv_data: HashMap<String, rmpv::Value> = data
            .into_iter()
            .map(|(k, v)| rmpv::ext::to_value(v).map(|rv| (k, rv)))
            .collect::<std::result::Result<_, _>>()
            .context("converting request data to rmpv")?;
        let request = QuicRequest {
            synapse_type: synapse_type.to_string(),
            data: rmpv_data,
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

        let json_map: serde_json::Map<String, serde_json::Value> = response
            .data
            .into_iter()
            .map(|(k, v)| serde_json::to_value(v).map(|jv| (k, jv)))
            .collect::<serde_json::Result<_>>()
            .context("converting response data from rmpv")?;
        Ok((serde_json::Value::Object(json_map), elapsed))
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
        let host = if ip.contains(':') {
            format!("[{ip}]")
        } else {
            ip.to_string()
        };
        let url = format!("http://{}:{}/{}", host, port, synapse_type);

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
