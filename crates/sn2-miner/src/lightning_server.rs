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
    let mut server = LightningServer::new(miner_hotkey.to_string(), host.to_string(), port)?;

    // btlightning's from_wallet passes (path, hotkey_name) to Wallet::new in
    // swapped positions: path→hotkey slot, hotkey_name→path slot. Compensate
    // by swapping here so the correct values reach bittensor_wallet::Wallet::new.
    server.set_miner_wallet(wallet_name, hotkey_name, wallet_path)?;

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

    let h = handlers;
    server
        .register_async_synapse_handler(
            Competition::NAME.to_string(),
            typed_async_handler(move |query: Competition| {
                let h = h.clone();
                async move { h.handle_competition(query).await }
            }),
        )
        .await?;

    server.start().await?;

    info!(host = host, port = port, "QUIC Lightning server listening");

    server.serve_forever().await?;
    Ok(())
}
