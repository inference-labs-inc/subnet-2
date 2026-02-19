use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "sn2-miner", about = "Subnet-2 Miner")]
pub struct Cli {
    #[arg(long, default_value_t = sn2_types::DEFAULT_NETUID)]
    pub netuid: u16,

    #[arg(long, default_value = "finney")]
    pub network: String,

    #[arg(long)]
    pub subtensor_chain_endpoint: Option<String>,

    #[arg(long, default_value = "default")]
    pub wallet_name: String,

    #[arg(long, default_value = "default")]
    pub wallet_hotkey: String,

    #[arg(long)]
    pub wallet_path: Option<String>,

    #[arg(long, default_value = "info")]
    pub log_level: String,

    #[arg(long, default_value = "0.0.0.0")]
    pub axon_host: String,

    #[arg(long, default_value_t = 8091)]
    pub axon_port: u16,

    #[arg(long, default_value_t = 8092)]
    pub quic_port: u16,

    #[arg(long)]
    pub external_ip: Option<String>,

    #[arg(long)]
    pub dsperse_socket: Option<String>,

    #[arg(long, default_value = "competition_circuit")]
    pub circuit_dir: String,

    #[arg(long)]
    pub storage_bucket: Option<String>,

    #[arg(long, default_value_t = false)]
    pub no_auto_update: bool,
}
