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

    #[arg(long, default_value_t = 20)]
    pub api_miners_pct: u32,

    #[arg(long, default_value_t = false)]
    pub disable_benchmark: bool,

    #[arg(long)]
    pub relay_url: Option<String>,

    #[arg(long, default_value_t = false)]
    pub no_relay: bool,

    #[arg(long, default_value_t = 9090)]
    pub metrics_port: u16,

    #[arg(long, default_value_t = false)]
    pub no_auto_update: bool,

    #[arg(long)]
    pub proof_api_url: Option<String>,

    #[arg(long, value_delimiter = ',')]
    pub target_uid: Vec<u16>,

    #[arg(long)]
    pub circuit_api_url: Option<String>,

    #[arg(
        long,
        env = "SN2_CIRCUIT_CACHE_DIR",
        help = "Directory for the persisted circuit cache. Defaults to \
                ~/.bittensor/subnet-2/circuit_cache when unset. May also be \
                supplied via the SN2_CIRCUIT_CACHE_DIR environment variable; \
                the CLI flag wins when both are present. A leading ~ is \
                expanded to the home directory."
    )]
    pub circuit_cache_dir: Option<String>,

    #[arg(long)]
    pub verification_concurrency: Option<usize>,

    #[arg(
        long,
        help = "Maximum number of concurrent in-flight miner queries. \
                Unset (default) is uncapped — adaptive per-miner caps and \
                pending_verifications buffer provide the backpressure. \
                Set to a positive integer to apply a hard system-wide cap, \
                useful for constrained hosts or controlled rollout."
    )]
    pub dispatch_ceiling: Option<usize>,

    #[arg(long, default_value_t = false)]
    pub disable_metric_logging: bool,

    #[arg(
        long,
        default_value_t = false,
        help = "Run without chain interaction for local integration testing"
    )]
    pub loopback: bool,

    #[arg(
        long,
        default_value = "127.0.0.1:8091",
        help = "Miner address (ip:port) for loopback mode"
    )]
    pub miner_address: String,

    #[arg(long, value_delimiter = ',')]
    pub additional_circuits: Vec<String>,

    #[arg(
        long,
        alias = "axon.external_ip",
        env = "BT_AXON_EXTERNAL_IP",
        help = "External IP to publish to the subtensor Axons map so miners can \
                enforce a source-IP allowlist. Auto-detected via api.ipify.org \
                when unset. May also be supplied via the BT_AXON_EXTERNAL_IP \
                environment variable; the CLI flag wins when both are present."
    )]
    pub external_ip: Option<String>,

    #[arg(
        long,
        alias = "axon.port",
        default_value_t = 8091,
        help = "Port published alongside external_ip in the Axons map. The \
                validator does not bind this port — it is a placeholder used \
                only so miners running source-IP allowlists can identify the \
                validator's hotkey by IP."
    )]
    pub axon_port: u16,

    #[arg(
        long,
        default_value_t = false,
        help = "Skip publishing the validator axon (IP + port) to the chain. \
                Use only when the validator runs behind an unstable egress IP \
                and cannot serve a stable source IP to miners."
    )]
    pub disable_axon_publish: bool,
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    fn min_args() -> Vec<&'static str> {
        vec!["sn2-validator"]
    }

    struct EnvRestore {
        var: &'static str,
        prior: Option<String>,
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(v) => std::env::set_var(self.var, v),
                None => std::env::remove_var(self.var),
            }
        }
    }

    #[test]
    fn external_ip_resolution_prefers_cli_then_env_then_unset() {
        // The env var and CLI flag share a single field; clap's resolution
        // rule is "CLI wins over env, env wins over default/unset". The three
        // assertions run inside one test so the env mutation isn't racing
        // with sibling parser invocations on other rules. EnvRestore is an
        // RAII guard that restores the operator's original value even if any
        // assertion below panics, so a failing assertion can't leak mutated
        // state into the rest of the test binary.
        let var = "BT_AXON_EXTERNAL_IP";
        let _guard = EnvRestore {
            var,
            prior: std::env::var(var).ok(),
        };

        std::env::remove_var(var);
        let cli = Cli::try_parse_from(min_args()).expect("parse without env");
        assert_eq!(cli.external_ip, None);

        std::env::set_var(var, "10.0.0.42");
        let cli = Cli::try_parse_from(min_args()).expect("parse with env only");
        assert_eq!(cli.external_ip.as_deref(), Some("10.0.0.42"));

        let mut args = min_args();
        args.extend_from_slice(&["--external-ip", "10.0.0.99"]);
        let cli = Cli::try_parse_from(args).expect("parse with flag overriding env");
        assert_eq!(cli.external_ip.as_deref(), Some("10.0.0.99"));
    }

    #[test]
    fn circuit_cache_dir_resolution_prefers_cli_then_env_then_unset() {
        // Mirrors external_ip: a single field backs both the CLI flag and the
        // SN2_CIRCUIT_CACHE_DIR env var, with CLI winning over env and env over
        // unset. Guarded by EnvRestore so a panicking assertion cannot leak the
        // mutated value into sibling tests in this binary.
        let var = "SN2_CIRCUIT_CACHE_DIR";
        let _guard = EnvRestore {
            var,
            prior: std::env::var(var).ok(),
        };

        std::env::remove_var(var);
        let cli = Cli::try_parse_from(min_args()).expect("parse without env");
        assert_eq!(cli.circuit_cache_dir, None);

        std::env::set_var(var, "/var/cache/sn2");
        let cli = Cli::try_parse_from(min_args()).expect("parse with env only");
        assert_eq!(cli.circuit_cache_dir.as_deref(), Some("/var/cache/sn2"));

        let mut args = min_args();
        args.extend_from_slice(&["--circuit-cache-dir", "/srv/circuits"]);
        let cli = Cli::try_parse_from(args).expect("parse with flag overriding env");
        assert_eq!(cli.circuit_cache_dir.as_deref(), Some("/srv/circuits"));
    }
}
