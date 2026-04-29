use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use btlightning::{LightningClient, QuicAxonInfo, QuicRequest, Signer};
use sn2_chain::Wallet;
use tracing::info;

use crate::cli::Cli;

const PHASE_HANDSHAKE: &str = "QUIC handshake + sr25519 signature";
const PHASE_QUERY: &str = "application-layer query";

struct WalletSigner(Arc<Wallet>);

impl Signer for WalletSigner {
    fn sign(&self, message: &[u8]) -> btlightning::Result<Vec<u8>> {
        self.0
            .sign_hotkey(message)
            .map_err(|e| btlightning::LightningError::Signing(e.to_string()))
    }
}

pub async fn run_self_probe(cli: Cli) -> Result<()> {
    let target = resolve_target(&cli)?;
    let wallet = Arc::new(
        Wallet::from_paths(
            &cli.wallet_name,
            &cli.wallet_hotkey,
            cli.wallet_path.as_deref(),
        )
        .context("loading wallet for self-probe")?,
    );

    let signer_hotkey = wallet.hotkey_ss58().to_string();
    let target_hotkey = cli
        .probe_target_hotkey
        .clone()
        .unwrap_or_else(|| signer_hotkey.clone());
    let probing_self = signer_hotkey == target_hotkey;

    println!("sn2-miner self-probe");
    println!("  signer hotkey  : {signer_hotkey}");
    println!("  target hotkey  : {target_hotkey}");
    println!("  target endpoint: {}:{}", target.ip, target.port);
    println!("  probing self   : {probing_self}");
    if probing_self {
        println!("  note           : application-layer query against your own hotkey is");
        println!("                   expected to be rejected by the validator-permit check.");
        println!("                   That rejection still confirms the network path works.");
    }
    println!();

    let timeout = Duration::from_secs(cli.probe_timeout);
    let mut client = build_client(signer_hotkey.clone(), wallet.clone())?;

    let axon = QuicAxonInfo::new(target_hotkey.clone(), target.ip.clone(), target.port, 4);

    println!("[1/3] {PHASE_HANDSHAKE} ...");
    let handshake_start = Instant::now();
    let handshake_outcome =
        tokio::time::timeout(timeout, client.initialize_connections(vec![axon.clone()])).await;
    let elapsed_ms = handshake_start.elapsed().as_millis();

    let handshake_ok = match handshake_outcome {
        Ok(Ok(_)) => {
            println!("      OK  ({elapsed_ms} ms)");
            true
        }
        Ok(Err(e)) => {
            println!("      FAIL ({elapsed_ms} ms): {e}");
            diagnose_handshake_error(&e.to_string());
            false
        }
        Err(_) => {
            println!("      TIMEOUT after {} s", cli.probe_timeout);
            diagnose_timeout();
            false
        }
    };

    if !handshake_ok {
        println!();
        println!(
            "verdict: axon is NOT completing the handshake. {PHASE_HANDSHAKE} did not succeed."
        );
        bail!("self-probe failed at handshake phase");
    }

    let synapse = match cli.probe_synapse.as_deref() {
        Some(s) => s,
        None => {
            println!();
            println!("[2/3] {PHASE_QUERY} ... skipped (pass --probe-synapse <name> to exercise)");
            println!("[3/3] verdict ...");
            println!(
                "      OK : axon is reachable, accepting connections, and validating signatures."
            );
            if probing_self {
                println!("      Validators with on-chain validator_permit will be admitted.");
            }
            return Ok(());
        }
    };

    println!();
    println!("[2/3] {PHASE_QUERY} (synapse = {synapse}) ...");
    let query_start = Instant::now();
    let query_outcome =
        tokio::time::timeout(timeout, run_synapse_query(&client, &axon, synapse, timeout)).await;
    let query_elapsed_ms = query_start.elapsed().as_millis();

    let mut permit_rejection = false;
    let query_ok = match query_outcome {
        Ok(Ok(_)) => {
            println!("      OK  ({query_elapsed_ms} ms): server returned a response");
            true
        }
        Ok(Err(e)) => {
            let msg = e.to_string();
            if looks_like_permit_rejection(&msg) {
                permit_rejection = true;
                println!("      REJECTED ({query_elapsed_ms} ms): {msg}");
                println!(
                    "      this rejection means the path works; the server reached its admit check."
                );
            } else {
                println!("      FAIL ({query_elapsed_ms} ms): {msg}");
            }
            false
        }
        Err(_) => {
            println!("      TIMEOUT after {} s", cli.probe_timeout);
            false
        }
    };

    println!();
    println!("[3/3] verdict ...");
    if query_ok {
        println!("      OK : axon is reachable, signatures valid, and application handler accepted the query.");
    } else if permit_rejection {
        println!(
            "      OK : axon is reachable. Application-layer query was rejected by the validator-permit"
        );
        println!(
            "           check, which is expected when probing your own hotkey. Real validators with"
        );
        println!("           an on-chain permit will be admitted.");
    } else {
        println!(
            "      MIXED: handshake succeeded but the application query did not. The network path"
        );
        println!("             is reachable; investigate the synapse handler or payload.");
    }

    if !query_ok && !permit_rejection {
        bail!("self-probe completed handshake but failed at application phase");
    }

    Ok(())
}

struct Target {
    ip: String,
    port: u16,
}

fn resolve_target(cli: &Cli) -> Result<Target> {
    if let Some(spec) = &cli.probe_target {
        let (ip, port_str) = spec
            .rsplit_once(':')
            .ok_or_else(|| anyhow!("--probe-target must be <ip:port>, got `{spec}`"))?;
        let port: u16 = port_str
            .parse()
            .with_context(|| format!("parsing port from `{spec}`"))?;
        return Ok(Target {
            ip: ip.to_string(),
            port,
        });
    }
    let ip = cli.external_ip.clone().ok_or_else(|| {
        anyhow!("--probe-target was not provided and --external-ip is unset; specify one")
    })?;
    Ok(Target {
        ip,
        port: cli.axon_port,
    })
}

fn build_client(hotkey: String, wallet: Arc<Wallet>) -> Result<LightningClient> {
    let config = btlightning::LightningClientConfig {
        max_frame_payload_bytes: sn2_types::TRANSPORT_PAYLOAD_LIMIT,
        max_stream_payload_bytes: sn2_types::TRANSPORT_PAYLOAD_LIMIT,
        ..Default::default()
    };
    let mut client = LightningClient::with_config(hotkey, config)
        .map_err(anyhow::Error::from)
        .context("building lightning client for self-probe")?;
    client.set_signer(Box::new(WalletSigner(wallet)));
    Ok(client)
}

async fn run_synapse_query(
    client: &LightningClient,
    axon: &QuicAxonInfo,
    synapse: &str,
    timeout: Duration,
) -> Result<()> {
    let data: HashMap<String, serde_json::Value> = HashMap::new();
    let request = QuicRequest::from_typed(synapse, &data)
        .map_err(anyhow::Error::from)
        .context("serializing probe payload")?;
    let response = client
        .query_axon_with_timeout(axon.clone(), request, timeout)
        .await
        .map_err(anyhow::Error::from)
        .context("query_axon_with_timeout")?;
    let _ = response.into_result().map_err(anyhow::Error::from)?;
    Ok(())
}

fn diagnose_handshake_error(msg: &str) {
    let m = msg.to_lowercase();
    if m.contains("connection refused") || m.contains("refused") {
        println!("      diagnosis: the host is reachable but the QUIC server isn't accepting on this UDP port.");
        println!(
            "                 Confirm the miner binary is running and bound to the same port."
        );
        println!("                 If running behind a NAT, confirm the public port forwards to the miner host.");
    } else if m.contains("no route") || m.contains("unreachable") {
        println!(
            "      diagnosis: the network path to the host is broken. Check routing / firewall."
        );
    } else if m.contains("timed out") || m.contains("timeout") {
        diagnose_timeout();
    } else if m.contains("signature") || m.contains("sr25519") || m.contains("invalid") {
        println!("      diagnosis: handshake reached the server but the signature was rejected.");
        println!("                 Verify the wallet identity and that the target hotkey matches the chain.");
    } else {
        println!("      diagnosis: handshake error not recognised; full message above.");
    }
    info!(error = %msg, "self-probe handshake error");
}

fn diagnose_timeout() {
    println!("      diagnosis: no QUIC handshake response within the timeout. UDP packets are most likely");
    println!("                 not reaching the miner process. Common causes:");
    println!(
        "                  - cloud security group / firewall not forwarding UDP on the axon port"
    );
    println!("                  - NAT / port-forward rule configured for TCP only");
    println!("                  - miner advertised an external IP that does not actually route");
    println!("                 On the miner host, run:  sudo tcpdump -i any -n -c 50 udp port <axon_port>");
    println!(
        "                 If no packets arrive while this probe is running, the issue is upstream."
    );
}

fn looks_like_permit_rejection(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("validator_permit")
        || m.contains("validator permit")
        || (m.contains("permit") && m.contains("requir"))
        || m.contains("not permitted")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli_with(probe_target: Option<&str>, external_ip: Option<&str>, axon_port: u16) -> Cli {
        Cli {
            netuid: 2,
            network: "test".into(),
            subtensor_chain_endpoint: None,
            wallet_name: "x".into(),
            wallet_hotkey: "x".into(),
            wallet_path: None,
            log_level: "info".into(),
            axon_host: "0.0.0.0".into(),
            axon_port,
            external_ip: external_ip.map(String::from),
            no_auto_update: true,
            disable_blacklist: false,
            metagraph_sync_interval: 600,
            loopback: false,
            self_probe: true,
            probe_target: probe_target.map(String::from),
            probe_target_hotkey: None,
            probe_timeout: 10,
            probe_synapse: None,
            additional_circuits: Vec::new(),
            handler_timeout: 180,
        }
    }

    #[test]
    fn target_explicit_overrides_external_ip() {
        let cli = cli_with(Some("1.2.3.4:9000"), Some("10.0.0.1"), 8091);
        let t = resolve_target(&cli).unwrap();
        assert_eq!(t.ip, "1.2.3.4");
        assert_eq!(t.port, 9000);
    }

    #[test]
    fn target_falls_back_to_external_ip_and_axon_port() {
        let cli = cli_with(None, Some("198.51.100.7"), 8091);
        let t = resolve_target(&cli).unwrap();
        assert_eq!(t.ip, "198.51.100.7");
        assert_eq!(t.port, 8091);
    }

    #[test]
    fn target_errors_when_no_source_provided() {
        let cli = cli_with(None, None, 8091);
        assert!(resolve_target(&cli).is_err());
    }

    #[test]
    fn target_rejects_malformed_spec() {
        let cli = cli_with(Some("not-an-endpoint"), None, 0);
        assert!(resolve_target(&cli).is_err());
        let cli = cli_with(Some("1.2.3.4:not-a-port"), None, 0);
        assert!(resolve_target(&cli).is_err());
    }

    #[test]
    fn permit_rejection_classifier_recognises_common_phrases() {
        assert!(looks_like_permit_rejection("validator_permit required"));
        assert!(looks_like_permit_rejection("Validator permit required"));
        assert!(looks_like_permit_rejection("not permitted"));
        assert!(looks_like_permit_rejection("permit is required"));
        assert!(!looks_like_permit_rejection("connection refused"));
        assert!(!looks_like_permit_rejection("timed out"));
    }
}
