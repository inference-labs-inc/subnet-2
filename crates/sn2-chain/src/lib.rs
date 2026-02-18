mod wallet;
mod metagraph;
mod weights;
mod registration;

pub use wallet::Wallet;
pub use metagraph::{Metagraph, NeuronInfo};
pub use weights::WeightsSetter;
pub use registration::Registration;

pub const FINNEY_ENDPOINT: &str = "wss://entrypoint-finney.opentensor.ai:443";
pub const TEST_ENDPOINT: &str = "wss://test.finney.opentensor.ai:443";
pub const LOCAL_ENDPOINT: &str = "ws://127.0.0.1:9944";
