pub const SOFTWARE_VERSION: &str = match option_env!("SN2_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

pub const VALIDATOR_REQUEST_TIMEOUT_SECONDS: u64 = 120;
pub const CIRCUIT_TIMEOUT_SECONDS: u64 = 180;
pub const MAX_CONCURRENT_REQUESTS: usize = 32;
pub const WEIGHTS_VERSION: u32 = 11003;
pub const WEIGHT_RATE_LIMIT_BLOCKS: u64 = 100;
pub const WEIGHT_UPDATE_POLL_SECS: u64 = 60;
pub const LOOP_DELAY_SECONDS: f64 = 0.1;
pub const EXCEPTION_DELAY_SECONDS: u64 = 10;
pub const DEFAULT_NETUID: u16 = 2;
pub const MAX_SIGNATURE_LIFESPAN: u64 = 300;

pub const BATCHED_PROOF_OF_WEIGHTS_MODEL_ID: &str =
    "1550853037e01d93c0831e2a4f80de7811b1c6780fb36b3cee89f4ba524df1be";

pub const PERFORMANCE_WINDOW_SIZE: usize = 2000;
pub const PERFORMANCE_CURVE_POWER: f64 = 3.0;
pub const PERFORMANCE_MIN_SAMPLES: usize = 5;
pub const PERFORMANCE_RESCHEDULE_PENALTY: f64 = -0.4;
pub const PERFORMANCE_SCORING_PERCENTILE: f64 = 0.50;
pub const CAPACITY_WINDOW_SIZE: usize = 20;
pub const CAPACITY_RAMP_THRESHOLD: f64 = 1.0;
pub const CAPACITY_BACKOFF_THRESHOLD: f64 = 0.50;
pub const CAPACITY_MIN_AT_CAP: usize = 8;

pub const MAX_POW_QUEUE_SIZE: usize = 1024;
pub const POW_OUTPUT_STRIDE: usize = MAX_POW_QUEUE_SIZE;
pub const POW_SCORES_OFFSET: usize = 0;
pub const POW_UIDS_OFFSET: usize = POW_OUTPUT_STRIDE * 2;
pub const POW_NUM_OUTPUT_ARRAYS: usize = 4;
pub const MAX_EVALUATION_ITEMS: usize = 1024;
pub const MAX_CIRCUIT_SIZE_GB: usize = 500;

pub const SN2_RELAY_URL: &str = "wss://sn2-relay.inferencelabs.com:8443";
pub const DEFAULT_API_URL: &str = "https://sn2-api.inferencelabs.com";
pub const CIRCUIT_API_URL: &str = "https://repository.inferencelabs.com";
pub const CIRCUIT_CACHE_DIR: &str = "~/.bittensor/subnet-2/circuit_cache";

pub const RELAY_RECONNECT_BASE_DELAY: u64 = 1;
pub const RELAY_RECONNECT_MAX_DELAY: u64 = 60;
pub const RELAY_AUTH_TIMEOUT: u64 = 10;
pub const RELAY_PING_INTERVAL: u64 = 30;
pub const RELAY_PING_TIMEOUT: u64 = 120;

pub const IGNORED_MODEL_HASHES: &[&str] = &[
    "0",
    "0a92bc32ea02abe54159da70aeb541d52c3cba27c8708669eda634e096a86f8b",
    "b7d33e7c19360c042d94c5a7360d7dc68c36dd56c449f7c49164a0098769c01f",
    "55de10a6bcf638af4bc79901d63204a9e5b1c6534670aa03010bae6045e3d0e8",
    "9998a12b8194d3e57d332b484ede57c3d871d42a176456c4e10da2995791d181",
    "ed8ba401d709ee31f6b9272163c71451da171c7d71800313fe5db58d0f6c483a",
    "37320fc74fec80805eedc8e92baf3c58842a2cb2a4ae127ad6e930f0c8441c7a",
    "1d60d545b7c5123fd60524dcbaf57081ca7dc4a9ec36c892927a3153328d17c0",
    "33b92394b18412622adad75733a6fc659b4e202b01ee8a5465958a6bad8ded62",
    "8dcff627a782525ea86196941a694ffbead179905f0cd4550ddc3df9e2b90924",
    "a4bcecaf699fd9212600a1f2fcaa40c444e1aeaab409ea240a38c33ed356f4e2",
    "e84b2e5f223621fa20078eb9f920d8d4d3a4ff95fa6e2357646fdbb43a2557c9",
    "a849500803abdbb86a9460e18684a6411dc7ae0b75f1f6330e3028081a497dea",
    "f5b6043594f46ae6bd176ce60c7a099291cc6a3f6436fecd46142b1b1ecca5fb",
    "1e6fcdaea58741e7248b631718dda90398a17b294480beb12ce8232e27ca3bff",
    "fa0d509d52abe2d1e809124f8aba46258a02f7253582f7b7f5a22e1e0bca0dfb",
];

pub const MAX_SLICE_RETRIES: u32 = 5;
pub const MAX_API_RETRIES: u32 = 20;
pub const API_TIMEOUT_SECONDS: f64 = 30.0;
pub const ADAPTIVE_TIMEOUT_MIN_SAMPLES: usize = 50;
pub const ADAPTIVE_TIMEOUT_MULTIPLIER: f64 = 2.0;
pub const ADAPTIVE_TIMEOUT_PERCENTILE: f64 = 0.95;
