use std::net::IpAddr;

use anyhow::{Context, Result};
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
        wallet: &Wallet,
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
            IpAddr::V6(v6) => {
                let segments = v6.segments();
                let mut ip_int: u128 = 0;
                for seg in segments {
                    ip_int = (ip_int << 16) | (seg as u128);
                }
                (ip_int as u64, 6u64)
            }
        };

        let tx = subxt::dynamic::tx(
            "SubtensorModule",
            "serve_axon",
            vec![
                Value::from(0u64),
                Value::from(ip_int),
                Value::from(port as u64),
                Value::from(ip_type),
                Value::from(self.netuid as u64),
                Value::from_bytes(wallet.hotkey_ss58.as_bytes()),
                Value::from_bytes(wallet.coldkey_ss58.as_bytes()),
                Value::from(protocol as u64),
                Value::from(0u64),
                Value::from(0u64),
            ],
        );

        let signer = crate::weights::SubxtSr25519Signer::new(wallet.hotkey.clone());

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
