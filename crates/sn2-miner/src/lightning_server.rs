use std::sync::Arc;

use anyhow::Result;
use btlightning::{typed_async_handler, LightningServer, LightningServerConfig};
use tracing::info;

use sn2_types::*;

use crate::handlers::MinerHandlers;

pub async fn run_lightning_server(
    miner_hotkey: &str,
    wallet_name: &str,
    wallet_path: &str,
    hotkey_name: &str,
    host: &str,
    port: u16,
    handler_timeout_secs: u64,
    handlers: Arc<MinerHandlers>,
) -> Result<()> {
    let idle_timeout = handler_timeout_secs.saturating_mul(2).max(150);
    let config = LightningServerConfig::builder()
        .handler_timeout_secs(handler_timeout_secs)
        .idle_timeout_secs(idle_timeout)
        .build()?;
    let mut server =
        LightningServer::with_config(miner_hotkey.to_string(), host.to_string(), port, config)?;

    server.set_miner_wallet(wallet_name, wallet_path, hotkey_name)?;

    let h = handlers.clone();
    server
        .register_async_synapse_handler(
            QueryZkProof::NAME.to_string(),
            typed_async_handler(move |query: QueryZkProof| {
                let h = h.clone();
                async move { h.handle_query_zk_proof(query).await }
            }),
        )
        .await?;

    let h = handlers.clone();
    server
        .register_async_synapse_handler(
            DSliceProofGenerationDataModel::NAME.to_string(),
            typed_async_handler(move |query: DSliceProofGenerationDataModel| {
                let h = h.clone();
                async move { h.handle_dslice(query).await }
            }),
        )
        .await?;

    server.start().await?;

    info!(host = host, port = port, "QUIC Lightning server listening");

    server.serve_forever().await?;
    Ok(())
}
