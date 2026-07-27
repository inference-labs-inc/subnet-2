pub mod attestation;
pub mod auto_update;
mod metagraph;
mod registration;
mod subxt_helpers;
mod wallet;
mod weights;

use std::sync::Arc;

use anyhow::{Context, Result};
use subxt::backend::{ChainHeadBackend, CombinedBackend, LegacyBackend};
use subxt::rpcs::RpcClient;
use subxt::{OnlineClient, PolkadotConfig};

pub use metagraph::{Metagraph, NeuronInfo};
pub use registration::Registration;
pub use wallet::Wallet;
pub use weights::WeightsSetter;

pub const FINNEY_ENDPOINT: &str = "wss://entrypoint-finney.opentensor.ai:443";
pub const TEST_ENDPOINT: &str = "wss://test.finney.opentensor.ai:443";
pub const LOCAL_ENDPOINT: &str = "ws://127.0.0.1:9944";

pub fn resolve_endpoint(network: &str, override_endpoint: Option<&str>) -> String {
    match override_endpoint {
        Some(ep) => ep.to_string(),
        None => match network {
            "finney" | "mainnet" => FINNEY_ENDPOINT.to_string(),
            "test" | "testnet" => TEST_ENDPOINT.to_string(),
            "local" | "localnet" => LOCAL_ENDPOINT.to_string(),
            other => other.to_string(),
        },
    }
}

/// Open a subxt `OnlineClient` against `endpoint`. `wss://` URLs use the
/// TLS-validating `from_url`; `ws://` URLs use `from_insecure_url`, which
/// subxt requires for non-TLS sockets even when reaching localhost or a
/// private substrate node.
///
/// The backend is assembled explicitly rather than via
/// `OnlineClient::from_url`, which builds a `CombinedBackend` that enables the
/// `archive_v1_*` backend whenever the node advertises those methods in
/// `rpc_methods`. Pruned subtensor nodes advertise the archive namespace but
/// answer every archive call with `Method not found (-32601)`, and the archive
/// backend builds its storage streams lazily, so the failure surfaces on first
/// poll rather than at construction and `CombinedBackend`'s per-call fallback
/// chain never gets the chance to retry against `chainHead` or the legacy
/// `state_*` methods. Omitting the archive backend keeps storage reads on the
/// two backends every subtensor node actually serves.
pub async fn connect_chain(endpoint: &str) -> Result<OnlineClient<PolkadotConfig>> {
    let rpc_client = if endpoint.starts_with("ws://") {
        RpcClient::from_insecure_url(endpoint).await
    } else {
        RpcClient::from_url(endpoint).await
    }
    .with_context(|| format!("connecting to subtensor at {endpoint}"))?;

    let chain_head = ChainHeadBackend::builder().build_with_background_driver(rpc_client.clone());
    let legacy = LegacyBackend::builder().build(rpc_client.clone());

    let backend = CombinedBackend::<PolkadotConfig>::builder()
        .no_default_backends()
        .with_chainhead_backend(chain_head)
        .with_legacy_backend(legacy)
        .build_with_background_driver(rpc_client)
        .await
        .with_context(|| format!("building subtensor backend for {endpoint}"))?;

    OnlineClient::<PolkadotConfig>::from_backend(Arc::new(backend))
        .await
        .with_context(|| format!("connecting to subtensor at {endpoint}"))
}

pub fn is_rpc_disconnect(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(subxt_err) = cause.downcast_ref::<subxt::Error>() {
            return subxt_err.is_disconnected_will_reconnect();
        }
    }
    false
}
