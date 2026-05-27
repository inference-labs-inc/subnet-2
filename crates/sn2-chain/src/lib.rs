pub mod attestation;
pub mod auto_update;
mod metagraph;
mod registration;
mod subxt_helpers;
mod wallet;
mod weights;

use anyhow::{Context, Result};
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
pub async fn connect_chain(endpoint: &str) -> Result<OnlineClient<PolkadotConfig>> {
    let result = if endpoint.starts_with("ws://") {
        OnlineClient::<PolkadotConfig>::from_insecure_url(endpoint).await
    } else {
        OnlineClient::<PolkadotConfig>::from_url(endpoint).await
    };
    result.with_context(|| format!("connecting to subtensor at {endpoint}"))
}

pub fn is_rpc_disconnect(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(subxt_err) = cause.downcast_ref::<subxt::Error>() {
            return subxt_err.is_disconnected_will_reconnect();
        }
    }
    false
}
