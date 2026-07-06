pub const SOFTWARE_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const IS_RELEASE_BUILD: bool = option_env!("SN2_RELEASE_CHANNEL").is_some();

pub const TRANSPORT_PAYLOAD_LIMIT: usize = 128 * 1024 * 1024;
pub const VALIDATOR_REQUEST_TIMEOUT_SECONDS: u64 = 120;
pub const CIRCUIT_TIMEOUT_SECONDS: u64 = 180;
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
pub const PERFORMANCE_CURVE_POWER: f64 = 1.3;
pub const PERFORMANCE_MIN_SAMPLES: usize = 100;
/// Half-life of the recency decay applied to delivered-work buckets when
/// computing the weight input. Recent verified delivery props the score up;
/// a miner that stops delivering sees its effective work halve every
/// half-life instead of coasting on the flat window sum, while brief
/// restarts cost nothing measurable.
pub const DELIVERED_WORK_HALF_LIFE_SECS: u64 = 2 * 3600;
pub const DELIVERED_WORK_BUCKET_SECS: u64 = 3600;
pub const FAILURE_DEBIT_MULTIPLIER: f64 = 2.0;
pub const DSLICE_QUEUE_LOW_WATERMARK: usize = 512;
pub const MAX_CONCURRENT_BENCHMARK_RUNS: usize = 8;
pub const EXTRA_RUN_MIN_AVAIL_MEM_RATIO: f64 = 0.35;
pub const DSLICE_QUEUE_LOW_WATERMARK_MAX: usize = 4096;

/// Tempos a hotkey is skiplisted when the validator finds it not connected.
/// A miner that cannot be reached delivers no proof, and the validator does
/// not inspect why the connection is gone: an honest restart and a deliberate
/// disconnect are scored identically, ignored and weighted zero for one epoch,
/// then retried. One tempo is a Bittensor epoch.
pub const DISCONNECT_SKIPLIST_TEMPOS: u64 = 1;

pub const VERIFICATION_SAMPLES_PER_TEMPO: u64 = 20;
pub const VERIFICATION_STRIKES_REQUIRED: u32 = 1;
pub const VERIFICATION_STRIKES_WINDOW_BLOCKS: u64 = 7200;
pub const VERIFICATION_SKIPLIST_TEMPOS: u64 = 20;
pub const VERIFICATION_COLDSTART_BLOCKS: u64 = 1800;
pub const VERIFICATION_WINDOW_BLOCKS: u64 = 7200;
pub const VERIFICATION_COLDSTART_RETENTION_BLOCKS: u64 = 50400;
pub const VERIFICATION_HISTORY_CAP: usize = 8192;
pub const BLOCK_TIME_SECS: u64 = 12;
pub const RSV_EXPECTED_SUBS_PER_TEMPO: u64 = 500;
pub const PERFORMANCE_RESCHEDULE_PENALTY: f64 = -0.5;
pub const CAPACITY_LATENCY_BUDGET_SECS: f64 = 0.75;
pub const CAPACITY_RATE_BIN_SECS: u64 = 15;
pub const CAPACITY_RATE_WINDOW_BINS: usize = 6;
pub const CAPACITY_RATE_FILTER_BINS: usize = 40;
pub const CAPACITY_STEP_FRACTION: f64 = 0.10;
/// Multiple of the knee in-flight depth (delivered rate x uncongested
/// service time) the cap targets. Above 1.0 so the miner always has queued
/// work to start the moment a unit completes; the excess is bounded so
/// measured latencies stay near the uncongested floor.
pub const CAPACITY_TARGET_HEADROOM: f64 = 1.5;
/// Fractional band above the target inside which the cap holds. Without it
/// the cap flaps one step up and down around the target every adjustment
/// interval, spamming capacity events at equilibrium.
pub const CAPACITY_TARGET_DEADBAND: f64 = 0.25;
pub const BUNDLE_CACHE_IDLE_TTL_SECS: u64 = 120;
pub const CAPACITY_ADJUST_INTERVAL_SECS: u64 = 15;
pub const CAPACITY_UNIT_REFERENCE_PERCENTILE: f64 = 0.1;
pub const CAPACITY_RAMP_MIN_AVAIL_MEM_RATIO: f64 = 0.20;
pub const CAPACITY_PRESSURE_BACKOFF_FACTOR: f64 = 0.10;
pub const IP_REGION_CAP_FRACTION: f64 = 0.25;

/// Block-height cooldown applied to disabled slices and to miners whose
/// adaptive capacity ratchets below 1. Approximately one mainnet epoch
/// (~72 minutes at 12s blocks) — long enough for transient miner faults
/// or chain-side reconnect storms to self-heal, short enough that a
/// recovered miner re-enters dispatch within the same scoring window.
pub const REHAB_BLOCKS: u64 = 360;

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
pub const MAX_CONCURRENT_UPLOADS: usize = 4;
pub const API_TIMEOUT_SECONDS: f64 = 30.0;
pub const ADAPTIVE_TIMEOUT_MIN_SAMPLES: usize = 50;
pub const ADAPTIVE_TIMEOUT_MULTIPLIER: f64 = 2.0;
pub const ADAPTIVE_TIMEOUT_PERCENTILE: f64 = 0.95;
