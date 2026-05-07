use std::sync::Arc;

use anyhow::Result;
use btlightning::{
    typed_async_handler, LightningServer, LightningServerConfig, SourceAddressResolver,
    ValidatorPermitResolver,
};
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
    permit_resolver: Option<Box<dyn ValidatorPermitResolver>>,
    source_resolver: Option<Box<dyn SourceAddressResolver>>,
) -> Result<()> {
    let idle_timeout = handler_timeout_secs.saturating_mul(2).max(150);
    let require_validator_permit = permit_resolver.is_some();
    let enforce_source_allowlist = source_resolver.is_some();
    let config = LightningServerConfig::builder()
        .handler_timeout_secs(handler_timeout_secs)
        .idle_timeout_secs(idle_timeout)
        .max_frame_payload_bytes(sn2_types::TRANSPORT_PAYLOAD_LIMIT)
        .require_validator_permit(require_validator_permit)
        .enforce_source_allowlist(enforce_source_allowlist)
        .require_address_validation(true)
        .build()?;
    let mut server =
        LightningServer::with_config(miner_hotkey.to_string(), host.to_string(), port, config)?;

    server.set_miner_wallet(wallet_name, wallet_path, hotkey_name)?;

    if let Some(resolver) = permit_resolver {
        server.set_validator_permit_resolver(resolver);
        info!("Validator permit enforcement enabled -- only hotkeys with on-chain validator_permit will be admitted");
    }

    if let Some(resolver) = source_resolver {
        server.set_source_address_resolver(resolver);
        info!("Source-address allowlist enforcement enabled -- non-validator IPs are dropped at the QUIC listener before any state is allocated");
    }

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

    let h = handlers.clone();
    server
        .register_async_synapse_handler(
            ProofOfWeightsDataModel::NAME.to_string(),
            typed_async_handler(move |query: ProofOfWeightsDataModel| {
                let h = h.clone();
                async move { h.handle_proof_of_weights(query).await }
            }),
        )
        .await?;

    server.start().await?;

    info!(host = host, port = port, "QUIC Lightning server listening");

    server.serve_forever().await?;
    Ok(())
}
