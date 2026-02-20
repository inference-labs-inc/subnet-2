mod circuit_manager;
mod cli;
mod dsperse;
mod handlers;
mod http_server;
mod lightning_server;
mod signature;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

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
        let _update_handle = sn2_chain::auto_update::spawn_update_loop("sn2-miner");
    }

    info!(
        netuid = cli.netuid,
        network = %cli.network,
        "starting sn2-miner"
    );

    let wallet = std::sync::Arc::new(
        sn2_chain::Wallet::from_paths(
            &cli.wallet_name,
            &cli.wallet_hotkey,
            cli.wallet_path.as_deref(),
        )
        .context("loading wallet")?,
    );

    let endpoint =
        cli.subtensor_chain_endpoint
            .clone()
            .unwrap_or_else(|| match cli.network.as_str() {
                "finney" | "mainnet" => sn2_chain::FINNEY_ENDPOINT.to_string(),
                "test" | "testnet" => sn2_chain::TEST_ENDPOINT.to_string(),
                "local" | "localnet" => sn2_chain::LOCAL_ENDPOINT.to_string(),
                other => other.to_string(),
            });

    let chain_client = subxt::OnlineClient::<subxt::PolkadotConfig>::from_url(&endpoint)
        .await
        .with_context(|| format!("connecting to subtensor at {endpoint}"))?;

    let registration = sn2_chain::Registration::new(cli.netuid);

    let mut metagraph = sn2_chain::Metagraph::new(cli.netuid);
    metagraph
        .sync(&chain_client)
        .await
        .context("initial metagraph sync")?;
    let metagraph = Arc::new(RwLock::new(metagraph));

    let external_ip: std::net::IpAddr = cli
        .external_ip
        .as_deref()
        .unwrap_or("0.0.0.0")
        .parse()
        .context("parsing external IP")?;

    let http_port = cli.axon_port;
    let quic_port = cli.quic_port;

    let dsperse = dsperse::DSperseClient::new(cli.dsperse_socket.clone());
    let circuit_mgr = std::sync::Arc::new(circuit_manager::CircuitManager::new(
        &cli.circuit_dir,
        cli.storage_bucket.as_deref(),
    ));

    let circuit_monitor = circuit_mgr.clone().start_monitor();

    let handlers = handlers::MinerHandlers::new(dsperse, circuit_mgr);
    let handlers = std::sync::Arc::new(handlers);

    let disable_blacklist = cli.disable_blacklist;

    let http_handle = {
        let handlers = handlers.clone();
        let hotkey_ss58 = wallet.hotkey_ss58().to_string();
        let axon_host = cli.axon_host.clone();
        let meta = metagraph.clone();
        tokio::spawn(async move {
            http_server::run_http_server(
                &axon_host,
                http_port,
                handlers,
                &hotkey_ss58,
                meta,
                disable_blacklist,
            )
            .await
        })
    };

    let quic_handle = {
        let handlers = handlers.clone();
        let hotkey = wallet.hotkey_ss58().to_string();
        let w_name = wallet.name.clone();
        let w_path = wallet.wallet_path.clone();
        let w_hotkey = wallet.hotkey_name.clone();
        tokio::spawn(async move {
            lightning_server::run_lightning_server(
                &hotkey, &w_name, &w_path, &w_hotkey, "0.0.0.0", quic_port, handlers,
            )
            .await
        })
    };

    registration
        .serve_axon(&chain_client, &wallet, external_ip, http_port, 4)
        .await
        .context("registering axon on chain")?;

    info!(
        hotkey = %wallet.hotkey_ss58(),
        http_port = http_port,
        quic_port = quic_port,
        "miner running"
    );

    let metagraph_sync = {
        let meta = metagraph.clone();
        let client = chain_client.clone();
        let netuid = cli.netuid;
        let sync_interval = cli.metagraph_sync_interval;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(sync_interval)).await;
                let mut fresh = sn2_chain::Metagraph::new(netuid);
                match fresh.sync(&client).await {
                    Ok(()) => {
                        *meta.write().await = fresh;
                    }
                    Err(e) => {
                        warn!(error = %e, "metagraph sync failed, retaining previous state");
                    }
                }
            }
        })
    };

    tokio::select! {
        r = http_handle => {
            r?.context("HTTP server")?;
        }
        r = quic_handle => {
            r?.context("QUIC server")?;
        }
        _ = circuit_monitor => {
            warn!("circuit monitor exited unexpectedly");
        }
        r = metagraph_sync => {
            match r {
                Ok(()) => warn!("metagraph sync loop exited unexpectedly"),
                Err(e) => error!(error = %e, "metagraph sync task panicked"),
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down miner");
        }
    }

    Ok(())
}
