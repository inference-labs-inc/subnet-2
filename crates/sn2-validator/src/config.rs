use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result};
use subxt::{OnlineClient, PolkadotConfig};
use tracing::{error, info};

use sn2_chain::{Metagraph, NeuronInfo, Wallet};

use crate::cli::Cli;

pub struct ValidatorConfig {
    pub netuid: u16,
    pub wallet: Option<Arc<Wallet>>,
    pub chain_client: Option<OnlineClient<PolkadotConfig>>,
    pub metagraph: Metagraph,
    pub user_uid: u16,
    pub relay_enabled: bool,
    pub relay_url: String,
    pub max_concurrency: usize,
    pub api_miners_pct: u32,
    pub disable_benchmark: bool,
    pub metrics_port: u16,
    pub proof_api_url: Option<String>,
    pub is_testnet: bool,
    pub max_benchmark_concurrent: Option<usize>,
    pub target_uids: Option<HashSet<u16>>,
    pub circuit_api_url: Option<String>,
    pub disable_metric_logging: bool,
    pub loopback: bool,
}

impl ValidatorConfig {
    pub async fn from_cli(cli: &Cli) -> Result<Self> {
        let endpoint =
            cli.subtensor_chain_endpoint
                .clone()
                .unwrap_or_else(|| match cli.network.as_str() {
                    "finney" | "mainnet" => sn2_chain::FINNEY_ENDPOINT.to_string(),
                    "test" | "testnet" => sn2_chain::TEST_ENDPOINT.to_string(),
                    "local" | "localnet" => sn2_chain::LOCAL_ENDPOINT.to_string(),
                    other => other.to_string(),
                });

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
                error!(
                    hotkey = %wallet.hotkey_ss58(),
                    netuid = cli.netuid,
                    network = %cli.network,
                    "Hotkey is not registered on subnet. Register with: btcli subnets register --netuid {} --network {}",
                    cli.netuid,
                    cli.network,
                );
                std::process::exit(1);
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
            metagraph,
            user_uid,
            relay_enabled: cli.relay_enabled,
            relay_url,
            max_concurrency: cli.max_concurrency,
            api_miners_pct: cli.api_miners_pct,
            disable_benchmark: cli.disable_benchmark,
            metrics_port: cli.metrics_port,
            proof_api_url: cli.proof_api_url.clone(),
            is_testnet: matches!(cli.network.as_str(), "test" | "testnet"),
            max_benchmark_concurrent: cli.max_benchmark_concurrent,
            target_uids: if cli.target_uid.is_empty() {
                None
            } else {
                Some(cli.target_uid.iter().copied().collect())
            },
            circuit_api_url: cli.circuit_api_url.clone(),
            disable_metric_logging: cli.disable_metric_logging,
            loopback: false,
        })
    }

    pub fn from_cli_loopback(cli: &Cli, miner_ip: &str, miner_port: u16) -> Self {
        let miner_neuron = NeuronInfo {
            uid: 0,
            hotkey: "loopback_miner".to_string(),
            coldkey: "loopback_coldkey".to_string(),
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

        Self {
            netuid: cli.netuid,
            wallet: None,
            chain_client: None,
            metagraph,
            user_uid: 1,
            relay_enabled: false,
            relay_url: String::new(),
            max_concurrency: cli.max_concurrency,
            api_miners_pct: 0,
            disable_benchmark: cli.disable_benchmark,
            metrics_port: cli.metrics_port,
            proof_api_url: None,
            is_testnet: true,
            max_benchmark_concurrent: cli.max_benchmark_concurrent,
            target_uids: Some([0].into_iter().collect()),
            circuit_api_url: cli.circuit_api_url.clone(),
            disable_metric_logging: true,
            loopback: true,
        }
    }
}
