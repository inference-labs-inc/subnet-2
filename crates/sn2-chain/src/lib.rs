pub mod auto_update;
mod metagraph;
mod registration;
mod wallet;
mod weights;

pub use metagraph::{Metagraph, NeuronInfo};
pub use registration::Registration;
pub use wallet::Wallet;
pub use weights::{PendingReveal, WeightsSetter};

pub const FINNEY_ENDPOINT: &str = "wss://entrypoint-finney.opentensor.ai:443";
pub const TEST_ENDPOINT: &str = "wss://test.finney.opentensor.ai:443";
pub const LOCAL_ENDPOINT: &str = "ws://127.0.0.1:9944";

pub fn is_rpc_disconnect(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(subxt_err) = cause.downcast_ref::<subxt::Error>() {
            return matches!(subxt_err, subxt::Error::Rpc(_));
        }
    }
    false
}
