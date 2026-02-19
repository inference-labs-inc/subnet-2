mod circuit_store;
mod cli;
mod config;
mod dsperse;
mod incremental_runner;
mod metrics_server;
mod miner_client;
mod performance;
mod proof_uploader;
mod relay;
mod request_pipeline;
mod response_processor;
mod scoring;
mod validator_loop;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use crate::cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::try_new(&cli.log_level).unwrap_or_else(|e| {
                    eprintln!("invalid --log-level \"{}\": {e}", cli.log_level);
                    std::process::exit(1);
                })
            }),
        )
        .init();

    if !cli.no_auto_update {
        let _update_handle = sn2_chain::auto_update::spawn_update_loop("sn2-validator");
    }

    info!(
        netuid = cli.netuid,
        network = %cli.network,
        "starting sn2-validator"
    );

    let config = config::ValidatorConfig::from_cli(&cli)
        .await
        .context("building validator config")?;

    let mut validator = validator_loop::ValidatorLoop::new(config)
        .await
        .context("building validator loop")?;
    validator.run().await
}
