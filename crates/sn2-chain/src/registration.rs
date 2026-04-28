use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use subxt::dynamic::Value;
use subxt::ext::scale_value::At;
use subxt::{OnlineClient, PolkadotConfig};
use tracing::info;

use crate::subxt_helpers::{at_current_block, fetch_value, netuid_hotkey_keys, PALLET};
use crate::wallet::Wallet;

pub struct Registration {
    netuid: u16,
}

impl Registration {
    pub fn new(netuid: u16) -> Self {
        Self { netuid }
    }

    pub async fn serve_axon(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        wallet: &Arc<Wallet>,
        ip: IpAddr,
        port: u16,
        protocol: u8,
    ) -> Result<()> {
        let (ip_int, ip_type) = match ip {
            IpAddr::V4(v4) => {
                let octets = v4.octets();
                let ip_int = ((octets[0] as u64) << 24)
                    | ((octets[1] as u64) << 16)
                    | ((octets[2] as u64) << 8)
                    | (octets[3] as u64);
                (ip_int, 4u64)
            }
            IpAddr::V6(_) => {
                anyhow::bail!("IPv6 is not supported for axon registration");
            }
        };

        let hotkey_bytes = wallet.hotkey_public_bytes()?;
        let at_block = at_current_block(client).await?;
        if let Some(v) = fetch_value(
            &at_block,
            "Axons",
            netuid_hotkey_keys(self.netuid, &hotkey_bytes),
        )
        .await
        .context("fetching existing Axons entry")?
        {
            let chain_ip = v.at("ip").and_then(|v| v.as_u128()).unwrap_or(0) as u64;
            let chain_port = v.at("port").and_then(|v| v.as_u128()).unwrap_or(0) as u16;
            let chain_protocol = v.at("protocol").and_then(|v| v.as_u128()).unwrap_or(0) as u8;

            if chain_ip == ip_int && chain_port == port && chain_protocol == protocol {
                info!(ip = %ip, port = port, "axon already registered on chain, skipping");
                return Ok(());
            }
        }

        let tx = subxt::dynamic::tx(
            PALLET,
            "serve_axon",
            vec![
                Value::from(self.netuid as u64),
                Value::from(0u64),
                Value::from(ip_int),
                Value::from(port as u64),
                Value::from(ip_type),
                Value::from(protocol as u64),
                Value::from(0u64),
                Value::from(0u64),
            ],
        );

        let signer = crate::weights::SubxtSr25519Signer::new(wallet)?;

        let mut tx_client = client.tx().await.context("acquiring transactions client")?;
        let result = tx_client
            .sign_and_submit_then_watch_default(&tx, &signer)
            .await
            .context("submitting serve_axon")?
            .wait_for_finalized_success()
            .await
            .context("serve_axon finalization")?;

        info!(
            block = %result.extrinsic_hash(),
            ip = %ip,
            port = port,
            "axon registered on chain"
        );

        Ok(())
    }
}
