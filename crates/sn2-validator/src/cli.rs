use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "sn2-validator", about = "Subnet-2 Validator")]
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

    #[arg(long, default_value_t = sn2_types::MAX_CONCURRENT_REQUESTS)]
    pub max_concurrency: usize,

    #[arg(long, default_value_t = 20)]
    pub api_miners_pct: u32,

    #[arg(long, default_value_t = false)]
    pub disable_benchmark: bool,

    #[arg(long)]
    pub relay_url: Option<String>,

    #[arg(long, default_value_t = false)]
    pub relay_enabled: bool,

    #[arg(long, default_value_t = 9090)]
    pub metrics_port: u16,

    #[arg(long)]
    pub dsperse_socket: Option<String>,

    #[arg(long, default_value_t = false)]
    pub no_auto_update: bool,
}
