use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use sp_core::crypto::Ss58Codec;
use subxt::dynamic::Value;
use subxt::{OnlineClient, PolkadotConfig};
use tracing::info;

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
        let coldkey_bytes = sp_core::crypto::AccountId32::from_ss58check(wallet.coldkey_ss58())
            .map_err(|e| anyhow::anyhow!("invalid coldkey SS58: {:?}", e))?;

        let tx = subxt::dynamic::tx(
            "SubtensorModule",
            "serve_axon",
            vec![
                Value::from(0u64),
                Value::from(ip_int),
                Value::from(port as u64),
                Value::from(ip_type),
                Value::from(self.netuid as u64),
                Value::from_bytes(hotkey_bytes),
                Value::from_bytes(AsRef::<[u8]>::as_ref(&coldkey_bytes)),
                Value::from(protocol as u64),
                Value::from(0u64),
                Value::from(0u64),
            ],
        );

        let signer = crate::weights::SubxtSr25519Signer::new(wallet)?;

        let result = client
            .tx()
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
