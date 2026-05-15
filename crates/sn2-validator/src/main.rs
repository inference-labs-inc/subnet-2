#![feature(ip)]

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// Mimalloc reads option env vars on first option access (lazy). The default
// `purge_delay` of ~1s churns the page tables on our high-frequency
// dispatch workload — observed 38k mmap/munmap syscalls per 3s on mainnet,
// re-introducing a single-thread bottleneck at the allocator layer. Setting
// the env var from a constructor that runs before main() (and crucially
// before the tokio runtime build) captures the desired cadence before any
// sustained allocation. Operators can still override by setting the env
// var explicitly in their process environment.
#[cfg(target_os = "linux")]
#[ctor::ctor]
fn configure_mimalloc_purge_delay() {
    // SAFETY: ctor runs single-threaded before main; no other thread can
    // race on environment state at this point.
    unsafe {
        if std::env::var_os("MIMALLOC_PURGE_DELAY").is_none() {
            std::env::set_var("MIMALLOC_PURGE_DELAY", "60000");
        }
    }
}

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
