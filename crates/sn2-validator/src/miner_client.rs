use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use btlightning::{LightningClient, QuicAxonInfo, QuicRequest, Signer};
use sn2_chain::Wallet;
use tracing::{debug, info};

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
        let mut lightning = LightningClient::new(wallet.hotkey_ss58().to_string());
        lightning.set_signer(Box::new(WalletSigner(wallet.clone())));

        Ok(Self { lightning })
    }

    pub fn new_unsigned() -> Result<Self> {
        let lightning = LightningClient::new("loopback".to_string());
        Ok(Self { lightning })
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

        let resp_body: serde_json::Value = response
            .deserialize_data()
            .map_err(anyhow::Error::from)
            .context("deserializing QUIC response")?;
        Ok((resp_body, elapsed))
    }
}
