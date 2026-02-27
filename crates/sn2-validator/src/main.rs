#![feature(ip)]

mod circuit_store;
mod cli;
mod config;
mod incremental_runner;
mod metrics_server;
mod miner_client;
mod performance;
mod pow_manager;
mod proof_uploader;
mod relay;
mod request_pipeline;
mod response_processor;
mod scoring;
mod tensor_json;
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

    if !cli.no_auto_update && option_env!("SN2_RELEASE_CHANNEL") == Some("mainnet") {
        let _update_handle = sn2_chain::auto_update::spawn_update_loop("sn2-validator");
    }

    let config = if cli.loopback {
        info!(
            netuid = cli.netuid,
            miner_address = %cli.miner_address,
            "starting sn2-validator in loopback mode (no chain interaction)"
        );

        let (ip, port) = parse_miner_address(&cli.miner_address)?;
        config::ValidatorConfig::from_cli_loopback(&cli, &ip, port)
    } else {
        info!(
            netuid = cli.netuid,
            network = %cli.network,
            "starting sn2-validator"
        );

        config::ValidatorConfig::from_cli(&cli)
            .await
            .context("building validator config")?
    };

    let mut validator = validator_loop::ValidatorLoop::new(config)
        .await
        .context("building validator loop")?;
    validator.run().await
}

fn parse_miner_address(addr: &str) -> Result<(String, u16)> {
    let parts: Vec<&str> = addr.rsplitn(2, ':').collect();
    anyhow::ensure!(
        parts.len() == 2,
        "miner-address must be ip:port, got: {addr}"
    );
    let port: u16 = parts[0].parse().context("parsing miner port")?;
    anyhow::ensure!(port > 0, "miner-address port must be > 0");
    let host = parts[1].trim();
    anyhow::ensure!(
        !host.is_empty(),
        "miner-address must be ip:port, host cannot be empty"
    );
    Ok((host.to_string(), port))
}
