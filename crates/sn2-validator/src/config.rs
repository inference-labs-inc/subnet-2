use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result};
use subxt::{OnlineClient, PolkadotConfig};
use tracing::info;

use sn2_chain::{Metagraph, NeuronInfo, Wallet};

use crate::cli::Cli;

pub struct ValidatorConfig {
    pub netuid: u16,
    pub wallet: Option<Arc<Wallet>>,
    pub chain_client: Option<OnlineClient<PolkadotConfig>>,
    pub chain_endpoint: String,
    pub metagraph: Metagraph,
    pub user_uid: u16,
    pub relay_enabled: bool,
    pub relay_url: String,
    pub relay_url_override: bool,
    pub api_miners_pct: u32,
    pub disable_benchmark: bool,
    pub metrics_port: u16,
    pub proof_api_url: Option<String>,
    pub target_uids: Option<HashSet<u16>>,
    pub circuit_api_url: Option<String>,
    pub disable_metric_logging: bool,
    pub loopback: bool,
    pub additional_circuits: Vec<String>,
}

impl ValidatorConfig {
    pub async fn from_cli(cli: &Cli) -> Result<Self> {
        let endpoint =
            sn2_chain::resolve_endpoint(&cli.network, cli.subtensor_chain_endpoint.as_deref());

        let chain_client = OnlineClient::<PolkadotConfig>::from_url(&endpoint)
            .await
            .with_context(|| format!("connecting to subtensor at {endpoint}"))?;

        let wallet = Arc::new(
            Wallet::from_paths(
                &cli.wallet_name,
                &cli.wallet_hotkey,
                cli.wallet_path.as_deref(),
            )
            .context("loading wallet")?,
        );

        let mut metagraph = Metagraph::new(cli.netuid);
        metagraph
            .sync(&chain_client)
            .await
            .context("initial metagraph sync")?;

        let user_uid = match metagraph.get_uid_by_hotkey(wallet.hotkey_ss58()) {
            Some(uid) => uid,
            None => {
                anyhow::bail!(
                    "hotkey {} is not registered on subnet {}. Register with: btcli subnets register --netuid {} --network {}",
                    wallet.hotkey_ss58(),
                    cli.netuid,
                    cli.netuid,
                    cli.network,
                );
            }
        };

        let relay_url = cli
            .relay_url
            .clone()
            .unwrap_or_else(|| sn2_types::SN2_RELAY_URL.to_string());

        Ok(Self {
            netuid: cli.netuid,
            wallet: Some(wallet),
            chain_client: Some(chain_client),
            chain_endpoint: endpoint,
            metagraph,
            user_uid,
            relay_enabled: !cli.no_relay,
            relay_url,
            relay_url_override: cli.relay_url.is_some(),
            api_miners_pct: cli.api_miners_pct,
            disable_benchmark: cli.disable_benchmark,
            metrics_port: cli.metrics_port,
            proof_api_url: cli.proof_api_url.clone(),
            target_uids: if cli.target_uid.is_empty() {
                None
            } else {
                Some(cli.target_uid.iter().copied().collect())
            },
            circuit_api_url: cli.circuit_api_url.clone(),
            disable_metric_logging: cli.disable_metric_logging,
            loopback: false,
            additional_circuits: cli.additional_circuits.clone(),
        })
    }

    pub fn from_cli_loopback(cli: &Cli, miner_ip: &str, miner_port: u16) -> Result<Self> {
        let wallet = Arc::new(
            Wallet::from_paths(
                &cli.wallet_name,
                &cli.wallet_hotkey,
                cli.wallet_path.as_deref(),
            )
            .context("loading wallet for loopback QUIC signing")?,
        );

        let miner_hotkey = wallet.hotkey_ss58().to_string();
        let miner_neuron = NeuronInfo {
            uid: 0,
            hotkey: miner_hotkey,
            coldkey: String::new(),
            hotkey_bytes: [0u8; 32],
            stake: 0,
            rank: 0,
            trust: 0,
            consensus: 0,
            incentive: 0,
            dividends: 0,
            emission: 0,
            is_active: true,
            last_update: 0,
            axon_ip: miner_ip.to_string(),
            axon_port: miner_port,
            axon_protocol: 0,
            validator_permit: false,
        };

        let metagraph = Metagraph::from_neurons(cli.netuid, vec![miner_neuron]);

        info!(
            miner_ip = miner_ip,
            miner_port = miner_port,
            "constructed loopback metagraph with synthetic miner neuron"
        );

        Ok(Self {
            netuid: cli.netuid,
            wallet: Some(wallet),
            chain_client: None,
            chain_endpoint: String::new(),
            metagraph,
            user_uid: 1,
            relay_enabled: false,
            relay_url: String::new(),
            relay_url_override: false,
            api_miners_pct: 0,
            disable_benchmark: cli.disable_benchmark,
            metrics_port: cli.metrics_port,
            proof_api_url: None,
            target_uids: Some([0].into_iter().collect()),
            circuit_api_url: cli.circuit_api_url.clone(),
            disable_metric_logging: true,
            loopback: true,
            additional_circuits: cli.additional_circuits.clone(),
        })
    }

    pub async fn reconnect_chain_client(&mut self) -> Result<()> {
        info!(endpoint = %self.chain_endpoint, "reconnecting to subtensor");
        let client = OnlineClient::<PolkadotConfig>::from_url(&self.chain_endpoint)
            .await
            .with_context(|| format!("reconnecting to subtensor at {}", self.chain_endpoint))?;
        self.chain_client = Some(client);
        info!("subtensor reconnection successful");
        Ok(())
    }
}
