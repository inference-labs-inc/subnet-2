pub mod attestation;
pub mod auto_update;
mod metagraph;
mod registration;
mod subxt_helpers;
mod wallet;
mod weights;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use subxt::backend::{ChainHeadBackend, CombinedBackend, LegacyBackend};
use subxt::rpcs::client::reconnecting_rpc_client::{
    PingConfig, RpcClient as ReconnectingRpcClient,
};
use subxt::rpcs::RpcClient;
use subxt::{OnlineClient, PolkadotConfig};

pub use metagraph::{Metagraph, NeuronInfo};
pub use registration::Registration;
pub use wallet::Wallet;
pub use weights::WeightsSetter;

pub const FINNEY_ENDPOINT: &str = "wss://entrypoint-finney.opentensor.ai:443";
pub const TEST_ENDPOINT: &str = "wss://test.finney.opentensor.ai:443";
pub const LOCAL_ENDPOINT: &str = "ws://127.0.0.1:9944";

pub const CHAIN_RPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
pub const CHAIN_RPC_PING_INTERVAL: Duration = Duration::from_secs(20);
pub const CHAIN_RPC_INACTIVE_LIMIT: Duration = Duration::from_secs(60);
pub const CHAIN_RPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

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

/// Open a subxt `OnlineClient` against `endpoint`. Both `wss://` (TLS
/// validated) and plain `ws://` (localhost or a private substrate node) URLs
/// are accepted by the reconnecting transport.
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
///
/// The RPC transport is subxt's reconnecting client with websocket pings: a
/// silent path failure (NAT mapping expiry, load balancer idle close) is
/// detected within `CHAIN_RPC_INACTIVE_LIMIT` and the socket is re-established
/// with exponential backoff instead of failing every call for the rest of the
/// process lifetime on a closed background task. Calls in flight on the old
/// socket surface `DisconnectedWillReconnect` (see `is_rpc_disconnect`); calls
/// issued while a reconnect is pending are queued and dispatched once the new
/// socket is up, and `CHAIN_RPC_REQUEST_TIMEOUT` only starts when a call is
/// dispatched, so callers that need a hard deadline wrap the call in their own
/// timeout (the transaction paths in `weights` and `registration` do).
/// Subscriptions, including transaction status watches, are not resumed across
/// a reconnect; `WeightsSetter::commit_timelocked_weights` reconciles a lost
/// watch against chain state before reporting failure. The reconnecting client
/// retries the initial connection with the same backoff it uses at runtime, so
/// the first connect is bounded by `CHAIN_RPC_CONNECT_TIMEOUT` to keep an
/// unreachable endpoint a startup error rather than an endless retry.
pub async fn connect_chain(endpoint: &str) -> Result<OnlineClient<PolkadotConfig>> {
    let reconnecting = tokio::time::timeout(
        CHAIN_RPC_CONNECT_TIMEOUT,
        ReconnectingRpcClient::builder()
            .request_timeout(CHAIN_RPC_REQUEST_TIMEOUT)
            .enable_ws_ping(
                PingConfig::new()
                    .ping_interval(CHAIN_RPC_PING_INTERVAL)
                    .inactive_limit(CHAIN_RPC_INACTIVE_LIMIT),
            )
            .build(endpoint),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "connecting to subtensor at {endpoint} timed out after {:?}",
            CHAIN_RPC_CONNECT_TIMEOUT
        )
    })?
    .with_context(|| format!("connecting to subtensor at {endpoint}"))?;
    let rpc_client = RpcClient::new(reconnecting);

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
