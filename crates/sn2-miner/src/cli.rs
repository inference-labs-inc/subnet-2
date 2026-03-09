use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "sn2-miner", about = "Subnet-2 Miner")]
pub struct Cli {
    #[arg(long, default_value_t = sn2_types::DEFAULT_NETUID)]
    pub netuid: u16,

    #[arg(long, alias = "subtensor.network", default_value = "finney")]
    pub network: String,

    #[arg(long, alias = "subtensor.chain_endpoint")]
    pub subtensor_chain_endpoint: Option<String>,

    #[arg(long, alias = "wallet.name", default_value = "default")]
    pub wallet_name: String,

    #[arg(long, alias = "wallet.hotkey", default_value = "default")]
    pub wallet_hotkey: String,

    #[arg(long, alias = "wallet.path")]
    pub wallet_path: Option<String>,

    #[arg(long, alias = "logging.level", default_value = "info")]
    pub log_level: String,

    #[arg(long, alias = "axon.host", default_value = "0.0.0.0")]
    pub axon_host: String,

    #[arg(long, alias = "axon.port", default_value_t = 8091)]
    pub axon_port: u16,

    #[arg(long, alias = "axon.external_ip")]
    pub external_ip: Option<String>,

    #[arg(long, default_value_t = false)]
    pub no_auto_update: bool,

    #[arg(
        long,
        default_value_t = false,
        help = "[TESTING ONLY] Disable validator permit checks — bypasses all on-chain permit enforcement"
    )]
    pub disable_blacklist: bool,

    #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(30..), help = "Metagraph sync interval in seconds")]
    pub metagraph_sync_interval: u64,

    #[arg(
        long,
        default_value_t = false,
        help = "Run without chain interaction for local integration testing"
    )]
    pub loopback: bool,

    #[arg(long, value_delimiter = ',')]
    pub additional_circuits: Vec<String>,

    #[arg(long, default_value_t = sn2_types::CIRCUIT_TIMEOUT_SECONDS, value_parser = clap::value_parser!(u64).range(1..))]
    pub handler_timeout: u64,
}
