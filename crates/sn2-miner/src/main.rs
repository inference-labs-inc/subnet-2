mod allowlist;
mod cli;
mod dsperse;
mod firewall;
mod handlers;
mod lightning_server;
mod nftables;
mod roster;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    sn2_types::init_tracing(&cli.log_level);

    info!(version = sn2_types::SOFTWARE_VERSION, "sn2-miner");

    if let Some(Command::Firewall { out }) = cli.command.clone() {
        return firewall::emit_nftables(
            cli.netuid,
            &cli.network,
            cli.subtensor_chain_endpoint.as_deref(),
            cli.axon_port,
            out,
        )
        .await;
    }

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

    let chain_client = sn2_chain::connect_chain(&endpoint).await?;

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

    let external_ip = match resolve_external_ip(cli.external_ip.as_deref()).await {
        Ok(ip) => Some(ip),
        Err(e) if cli.external_ip.is_none() => {
            warn!(
                error = ?e,
                "external IP autodetection failed; skipping serve_axon for this boot"
            );
            None
        }
        Err(e) => return Err(e),
    };

    let quic_port = cli.axon_port;
    anyhow::ensure!(quic_port != 0, "QUIC port must be non-zero");

    let dsperse = dsperse::DSperseClient::new();

    let circuit_store = init_circuit_store(false, &cli.additional_circuits).await;

    let handlers = handlers::MinerHandlers::new(dsperse, circuit_store);
    let handlers = std::sync::Arc::new(handlers);

    let handler_timeout = cli.handler_timeout;
    let allowlist: Option<Arc<allowlist::ValidatorAllowlist>> = if cli.disable_blacklist {
        warn!("--disable-blacklist set; validator allowlist is bypassed (TESTING ONLY)");
        None
    } else {
        let cache_policy = if cli.no_validator_ip_cache {
            allowlist::CachePolicy::InMemoryOnly
        } else {
            allowlist::CachePolicy::PersistToDisk
        };
        Some(Arc::new(
            allowlist::ValidatorAllowlist::new(
                metagraph.clone(),
                cli.netuid,
                std::path::PathBuf::from(wallet.wallet_path.as_str()),
                cache_policy,
            )
            .context("initializing validator allowlist")?,
        ))
    };

    let nftables_manager = if cli.no_nftables || allowlist.is_none() {
        None
    } else {
        Some(Arc::new(nftables::NftablesManager::new(quic_port)))
    };

    let quic_handle = {
        let handlers = handlers.clone();
        let hotkey = wallet.hotkey_ss58().to_string();
        let w_name = wallet.name.clone();
        let w_path = wallet.wallet_path.clone();
        let w_hotkey = wallet.hotkey_name.clone();
        let allowlist = allowlist.clone();
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
                allowlist,
            )
            .await
        })
    };

    if let Some(external_ip) = external_ip {
        match registration
            .serve_axon(&chain_client, &wallet, external_ip, quic_port, 4)
            .await
        {
            Ok(()) => {}
            Err(e) => {
                warn!(error = %e, "serve_axon failed (rate-limited or transient); miner will continue");
            }
        }
    }

    info!(
        hotkey = %wallet.hotkey_ss58(),
        quic_port = quic_port,
        "miner running"
    );

    let metagraph_sync = {
        let meta = metagraph.clone();
        let client = chain_client.clone();
        let netuid = cli.netuid;
        let sync_interval = cli.metagraph_sync_interval;
        let allowlist = allowlist.clone();
        let nftables_manager = nftables_manager.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(sync_interval)).await;
                let mut fresh = sn2_chain::Metagraph::new(netuid);
                match fresh.sync(&client).await {
                    Ok(()) => {
                        *meta.write().await = fresh;
                        if let Some(al) = allowlist.as_ref() {
                            let cov = al.evaluate().await;
                            if let Some(nft) = nftables_manager.as_ref() {
                                nft.apply(cov.enforcing, &cov.allowed_ips).await;
                            }
                            info!(
                                enforcing = cov.enforcing,
                                coverage_pct = cov.fraction() * 100.0,
                                kappa_pct = cov.kappa_fraction() * 100.0,
                                blocks_since_start = cov.blocks_since_start,
                                tempo = cov.tempo,
                                "validator allowlist evaluated"
                            );
                        }
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

    if let Some(nft) = nftables_manager.as_ref() {
        nft.apply(false, &std::collections::HashSet::new()).await;
    }

    Ok(())
}

async fn run_loopback(cli: Cli) -> Result<()> {
    info!(
        port = cli.axon_port,
        "starting miner in loopback mode (no chain interaction)"
    );

    let wallet = sn2_chain::Wallet::from_paths(
        &cli.wallet_name,
        &cli.wallet_hotkey,
        cli.wallet_path.as_deref(),
    )
    .context("loading wallet")?;

    let dsperse = dsperse::DSperseClient::new();

    let circuit_store = init_circuit_store(true, &cli.additional_circuits).await;

    let handlers = handlers::MinerHandlers::new(dsperse, circuit_store);
    let handlers = std::sync::Arc::new(handlers);

    let handler_timeout = cli.handler_timeout;
    let quic_handle = {
        let handlers = handlers.clone();
        let host = cli.axon_host.clone();
        let port = cli.axon_port;
        let hotkey_ss58 = wallet.hotkey_ss58().to_string();
        let w_name = wallet.name.clone();
        let w_path = wallet.wallet_path.clone();
        let w_hotkey = wallet.hotkey_name.clone();
        tokio::spawn(async move {
            lightning_server::run_lightning_server(
                &hotkey_ss58,
                &w_name,
                &w_path,
                &w_hotkey,
                &host,
                port,
                handler_timeout,
                handlers,
                None,
            )
            .await
        })
    };

    info!(port = cli.axon_port, "miner loopback running");

    let mut sigterm = signal(SignalKind::terminate()).context("registering SIGTERM handler")?;

    tokio::select! {
        r = quic_handle => {
            r?.context("QUIC server")?;
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

async fn resolve_external_ip(override_ip: Option<&str>) -> Result<IpAddr> {
    if let Some(ip) = override_ip {
        let parsed: IpAddr = ip.parse().context("parsing --external-ip")?;
        return require_ipv4(parsed);
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("building HTTP client for external-IP detection")?;
    let resp = client
        .get("https://api4.ipify.org")
        .send()
        .await
        .context("detecting external IP via api4.ipify.org")?
        .text()
        .await
        .context("reading external IP response body")?;
    let parsed: IpAddr = resp
        .trim()
        .parse()
        .with_context(|| format!("parsing detected IP: {resp}"))?;
    require_ipv4(parsed)
}

fn require_ipv4(ip: IpAddr) -> Result<IpAddr> {
    match ip {
        IpAddr::V4(_) => Ok(ip),
        IpAddr::V6(_) => {
            anyhow::bail!(
                "external IP must be IPv4 (axon registration does not support IPv6): {ip}"
            )
        }
    }
}

async fn init_circuit_store(
    loopback: bool,
    additional_circuits: &[String],
) -> sn2_circuit_store::CircuitStore {
    let mut store =
        sn2_circuit_store::CircuitStore::new(None, loopback, additional_circuits.to_vec());
    if let Err(e) = store.load_circuits().await {
        warn!(error = %e, "failed to load circuits from cache");
    }
    for id in additional_circuits {
        if let Err(e) = store.ensure_circuit(id).await {
            warn!(id = %id, error = %e, "failed to preload pinned circuit");
        }
    }
    store
}
