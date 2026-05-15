#![feature(ip)]

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod cli;
mod config;
mod dsperse_events;
mod incremental_runner;
mod metrics_server;
mod miner_client;
mod performance;
mod proof_uploader;
mod relay;
mod request_pipeline;
mod response_processor;
mod rsv;
mod scoring;
mod stats_reporter;
mod tensor;
mod validator_loop;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::watch;
use tracing::info;

use crate::cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls CryptoProvider");

    let cli = Cli::parse();

    sn2_types::init_tracing(&cli.log_level);

    info!(version = sn2_types::SOFTWARE_VERSION, "sn2-validator");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    if !cli.no_auto_update && option_env!("SN2_RELEASE_CHANNEL") == Some("mainnet") {
        let _update_handle =
            sn2_chain::auto_update::spawn_update_loop("sn2-validator", shutdown_tx.clone());
    }

    let config = if cli.loopback {
        info!(
            netuid = cli.netuid,
            miner_address = %cli.miner_address,
            "starting sn2-validator in loopback mode (no chain interaction)"
        );

        let (ip, port) = parse_miner_address(&cli.miner_address)?;
        config::ValidatorConfig::from_cli_loopback(&cli, &ip, port)?
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
    validator.run(shutdown_rx).await
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
