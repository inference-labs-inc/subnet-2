mod cli;
mod dsperse;
mod handlers;
mod http_server;
mod lightning_server;
mod signature;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    sn2_types::init_tracing(&cli.log_level);

    info!(version = sn2_types::SOFTWARE_VERSION, "sn2-miner");

    if cli.loopback {
        return run_loopback(cli).await;
    }

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    if !cli.no_auto_update && option_env!("SN2_RELEASE_CHANNEL") == Some("mainnet") {
        let _update_handle =
            sn2_chain::auto_update::spawn_update_loop("sn2-miner", shutdown_tx.clone());
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
        sn2_chain::resolve_endpoint(&cli.network, cli.subtensor_chain_endpoint.as_deref());

    let chain_client = subxt::OnlineClient::<subxt::PolkadotConfig>::from_url(&endpoint)
        .await
        .with_context(|| format!("connecting to subtensor at {endpoint}"))?;

    let registration = sn2_chain::Registration::new(cli.netuid);

    let mut metagraph = sn2_chain::Metagraph::new(cli.netuid);
    metagraph
        .sync(&chain_client)
        .await
        .context("initial metagraph sync")?;

    anyhow::ensure!(
        metagraph.get_uid_by_hotkey(wallet.hotkey_ss58()).is_some(),
        "hotkey {} is not registered on subnet {}. Register with: btcli subnets register --netuid {} --network {}",
        wallet.hotkey_ss58(),
        cli.netuid,
        cli.netuid,
        cli.network,
    );

    let metagraph = Arc::new(RwLock::new(metagraph));

    let external_ip: std::net::IpAddr = match cli.external_ip.as_deref() {
        Some(ip) => ip.parse().context("parsing external IP")?,
        None => {
            let resp = reqwest::get("https://api.ipify.org")
                .await
                .context("detecting external IP")?
                .text()
                .await
                .context("reading external IP response")?;
            resp.trim()
                .parse()
                .with_context(|| format!("parsing detected IP: {resp}"))?
        }
    };

    let http_port = cli.axon_port;
    let quic_port = cli.quic_port.unwrap_or(cli.axon_port);
    anyhow::ensure!(quic_port != 0, "QUIC port must be non-zero");

    let dsperse = dsperse::DSperseClient::new();

    let circuit_store = init_circuit_store(false, &cli.additional_circuits).await;

    let handlers = handlers::MinerHandlers::new(dsperse, circuit_store);
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
                false,
            )
            .await
        })
    };

    let handler_timeout = cli.handler_timeout;
    let quic_handle = {
        let handlers = handlers.clone();
        let hotkey = wallet.hotkey_ss58().to_string();
        let w_name = wallet.name.clone();
        let w_path = wallet.wallet_path.clone();
        let w_hotkey = wallet.hotkey_name.clone();
        tokio::spawn(async move {
            lightning_server::run_lightning_server(
                &hotkey,
                &w_name,
                &w_path,
                &w_hotkey,
                "0.0.0.0",
                quic_port,
                handler_timeout,
                handlers,
            )
            .await
        })
    };

    match registration
        .serve_axon(&chain_client, &wallet, external_ip, quic_port, 4)
        .await
    {
        Ok(()) => {}
        Err(e) => {
            warn!(error = %e, "serve_axon failed (rate-limited or transient); miner will continue");
        }
    }

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

    let mut sigterm = signal(SignalKind::terminate()).context("registering SIGTERM handler")?;

    tokio::select! {
        r = http_handle => {
            r?.context("HTTP server")?;
        }
        r = quic_handle => {
            r?.context("QUIC server")?;
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
        _ = sigterm.recv() => {
            info!("received SIGTERM, shutting down miner");
        }
        _ = async { loop { shutdown_rx.changed().await.ok()?; if *shutdown_rx.borrow() { return Some(()); } } } => {
            info!("shutting down miner for auto-update restart");
        }
    }

    Ok(())
}

async fn run_loopback(cli: Cli) -> Result<()> {
    info!(
        port = cli.axon_port,
        "starting miner in loopback mode (no chain interaction)"
    );

    let dsperse = dsperse::DSperseClient::new();

    let circuit_store = init_circuit_store(true, &cli.additional_circuits).await;

    let handlers = handlers::MinerHandlers::new(dsperse, circuit_store);
    let handlers = std::sync::Arc::new(handlers);

    let metagraph = Arc::new(RwLock::new(sn2_chain::Metagraph::new(cli.netuid)));

    let http_handle = {
        let handlers = handlers.clone();
        let axon_host = cli.axon_host.clone();
        let port = cli.axon_port;
        let meta = metagraph.clone();
        tokio::spawn(async move {
            http_server::run_http_server(&axon_host, port, handlers, "loopback", meta, true, true)
                .await
        })
    };

    info!(port = cli.axon_port, "miner loopback running");

    let mut sigterm = signal(SignalKind::terminate()).context("registering SIGTERM handler")?;

    tokio::select! {
        r = http_handle => {
            r?.context("HTTP server")?;
        }
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down miner");
        }
        _ = sigterm.recv() => {
            info!("received SIGTERM, shutting down miner");
        }
    }

    Ok(())
}

async fn init_circuit_store(
    loopback: bool,
    additional_circuits: &[String],
) -> sn2_circuit_store::CircuitStore {
    let mut store =
        sn2_circuit_store::CircuitStore::new(None, loopback, additional_circuits.to_vec());
    for id in additional_circuits {
        if let Err(e) = store.ensure_circuit(id).await {
            warn!(id = %id, error = %e, "failed to preload pinned circuit");
        }
    }
    store
}
