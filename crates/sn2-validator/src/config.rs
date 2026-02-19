use std::sync::Arc;

use anyhow::{Context, Result};
use subxt::{OnlineClient, PolkadotConfig};

use sn2_chain::{Metagraph, Wallet};

use crate::cli::Cli;

pub struct ValidatorConfig {
    pub netuid: u16,
    pub wallet: Arc<Wallet>,
    pub chain_client: OnlineClient<PolkadotConfig>,
    pub metagraph: Metagraph,
    pub user_uid: u16,
    pub relay_enabled: bool,
    pub relay_url: String,
    pub max_concurrency: usize,
    pub api_miners_pct: u32,
    pub disable_benchmark: bool,
    pub metrics_port: u16,
    pub dsperse_socket: Option<String>,
    pub proof_api_url: Option<String>,
    pub is_testnet: bool,
    pub max_benchmark_concurrent: Option<usize>,
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

        let user_uid = metagraph
            .get_uid_by_hotkey(wallet.hotkey_ss58())
            .context("validator hotkey not registered in metagraph")?;

        let relay_url = cli
            .relay_url
            .clone()
            .unwrap_or_else(|| sn2_types::SN2_RELAY_URL.to_string());

        Ok(Self {
            netuid: cli.netuid,
            wallet,
            chain_client,
            metagraph,
            user_uid,
            relay_enabled: cli.relay_enabled,
            relay_url,
            max_concurrency: cli.max_concurrency,
            api_miners_pct: cli.api_miners_pct,
            disable_benchmark: cli.disable_benchmark,
            metrics_port: cli.metrics_port,
            dsperse_socket: cli.dsperse_socket.clone(),
            proof_api_url: cli.proof_api_url.clone(),
            is_testnet: matches!(cli.network.as_str(), "test" | "testnet"),
            max_benchmark_concurrent: cli.max_benchmark_concurrent,
        })
    }
}
