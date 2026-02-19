use std::sync::Arc;

use anyhow::Result;
use btlightning::{typed_async_handler, LightningServer};
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
    handlers: Arc<MinerHandlers>,
) -> Result<()> {
    let mut server = LightningServer::new(miner_hotkey.to_string(), host.to_string(), port);

    server
        .set_miner_wallet(wallet_name, wallet_path, hotkey_name)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let h = handlers.clone();
    server
        .register_async_synapse_handler(
            QueryZkProof::NAME.to_string(),
            typed_async_handler(move |query: QueryZkProof| {
                let h = h.clone();
                async move { h.handle_query_zk_proof(query).await }
            }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let h = handlers.clone();
    server
        .register_async_synapse_handler(
            DSliceProofGenerationDataModel::NAME.to_string(),
            typed_async_handler(move |query: DSliceProofGenerationDataModel| {
                let h = h.clone();
                async move { h.handle_dslice(query).await }
            }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let h = handlers;
    server
        .register_async_synapse_handler(
            Competition::NAME.to_string(),
            typed_async_handler(move |query: Competition| {
                let h = h.clone();
                async move { h.handle_competition(query).await }
            }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    server.start().await.map_err(|e| anyhow::anyhow!("{e}"))?;

    info!(host = host, port = port, "QUIC Lightning server listening");

    server
        .serve_forever()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
