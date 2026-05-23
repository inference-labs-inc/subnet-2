use std::sync::Arc;

use anyhow::Result;
use btlightning::{typed_async_handler, LightningServer, LightningServerConfig};
use tracing::info;

use sn2_types::*;

use crate::allowlist::ValidatorAllowlist;
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
    allowlist: Option<Arc<ValidatorAllowlist>>,
) -> Result<()> {
    let idle_timeout = handler_timeout_secs.saturating_mul(2).max(150);
    let require_validator_permit = allowlist.is_some();
    let enforce_source_allowlist = allowlist.is_some();
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

    if let Some(allowlist) = allowlist {
        // The allowlist implements all three traits: ValidatorPermitResolver
        // (handshake-time hotkey check), SourceAddressResolver (QUIC-listener
        // source-IP drop), and HandshakeObserver (trust-on-first-use roster
        // updates). Cloning the Arc keeps a single shared state machine across
        // all three roles.
        server.set_validator_permit_resolver(Box::new(arc_as_boxed_permit(allowlist.clone())));
        server.set_source_address_resolver(Box::new(arc_as_boxed_source(allowlist.clone())));
        server.set_handshake_observer(allowlist);
        info!(
            "Validator allowlist active: permit enforcement at handshake, trust-on-first-use source-IP allowlist gated by stake-weighted coverage"
        );
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

/// Trait adapter: the lightning server's permit/source resolver setters take
/// `Box<dyn Trait>`, but ValidatorAllowlist is owned through `Arc` so the same
/// state machine drives all three behaviors. These thin wrappers forward the
/// trait calls to the shared Arc.
struct ArcPermitResolver(Arc<ValidatorAllowlist>);
impl btlightning::ValidatorPermitResolver for ArcPermitResolver {
    fn resolve_permitted_validators(
        &self,
    ) -> btlightning::Result<std::collections::HashSet<String>> {
        self.0.resolve_permitted_validators()
    }
}

struct ArcSourceResolver(Arc<ValidatorAllowlist>);
impl btlightning::SourceAddressResolver for ArcSourceResolver {
    fn resolve_allowed_sources(&self) -> btlightning::Result<btlightning::SourceAllowlist> {
        self.0.resolve_allowed_sources()
    }
}

fn arc_as_boxed_permit(arc: Arc<ValidatorAllowlist>) -> ArcPermitResolver {
    ArcPermitResolver(arc)
}

fn arc_as_boxed_source(arc: Arc<ValidatorAllowlist>) -> ArcSourceResolver {
    ArcSourceResolver(arc)
}
