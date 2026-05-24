//! nftables ruleset manager driven off the validator allowlist state machine.
//!
//! Behavior:
//! * `Enforcing` -- renders an `add table; delete table; table { ... }` ruleset
//!   from the current roster and applies it via `nft -f -`. The pattern is
//!   atomic from the kernel's perspective.
//! * `Learning` -- removes the table (idempotent) so no kernel-level drops
//!   apply while the application layer is still in bypass.
//!
//! Linux-only. On other targets the type compiles but `apply` is a no-op so
//! development environments don't need root-level nftables. Failures from `nft`
//! are logged at `warn` and do not abort the miner.

use std::collections::{BTreeSet, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;

use tracing::{debug, info, warn};

#[cfg(not(target_os = "linux"))]
#[allow(unused_imports)]
use crate::firewall::render_ruleset;
#[cfg(target_os = "linux")]
use crate::firewall::{render_ruleset, NFT_TABLE_NAME};

pub struct NftablesManager {
    axon_port: u16,
    /// The most recently applied state, used to suppress redundant `nft` invocations.
    last: Mutex<Option<AppliedState>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppliedState {
    enforcing: bool,
    v4: BTreeSet<Ipv4Addr>,
    v6: BTreeSet<Ipv6Addr>,
}

impl NftablesManager {
    pub fn new(axon_port: u16) -> Self {
        Self {
            axon_port,
            last: Mutex::new(None),
        }
    }

    pub async fn apply(&self, enforcing: bool, allowed_ips: &HashSet<IpAddr>) {
        let (v4, v6) = split_ips(allowed_ips);
        let next = AppliedState {
            enforcing,
            v4: v4.clone(),
            v6: v6.clone(),
        };
        if self
            .last
            .lock()
            .expect("nftables state lock poisoned")
            .as_ref()
            == Some(&next)
        {
            debug!("nftables ruleset unchanged; skipping nft invocation");
            return;
        }
        let axon_port = self.axon_port;
        let result = tokio::task::spawn_blocking(move || {
            if enforcing {
                apply_ruleset(axon_port, &v4, &v6)
            } else {
                tear_down_table()
            }
        })
        .await;
        match result {
            Ok(Ok(())) => {
                if enforcing {
                    info!(
                        v4 = next.v4.len(),
                        v6 = next.v6.len(),
                        "nftables ruleset applied"
                    );
                } else {
                    info!("nftables table removed (Learning mode)");
                }
                *self.last.lock().expect("nftables state lock poisoned") = Some(next);
            }
            Ok(Err(e)) => {
                warn!(error = %e, "nftables update failed; userspace source-IP check remains in effect")
            }
            Err(e) => warn!(error = %e, "nftables task panicked"),
        }
    }
}

fn split_ips(ips: &HashSet<IpAddr>) -> (BTreeSet<Ipv4Addr>, BTreeSet<Ipv6Addr>) {
    let mut v4 = BTreeSet::new();
    let mut v6 = BTreeSet::new();
    for ip in ips {
        match ip {
            IpAddr::V4(a) => {
                v4.insert(*a);
            }
            IpAddr::V6(a) => {
                v6.insert(*a);
            }
        }
    }
    (v4, v6)
}

#[cfg(target_os = "linux")]
fn apply_ruleset(
    axon_port: u16,
    v4: &BTreeSet<Ipv4Addr>,
    v6: &BTreeSet<Ipv6Addr>,
) -> anyhow::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let ruleset = render_ruleset(axon_port, v4, v6);
    let mut child = Command::new("nft")
        .arg("-f")
        .arg("-")
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(ruleset.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        anyhow::bail!(
            "nft exited with status {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn tear_down_table() -> anyhow::Result<()> {
    use std::process::Command;
    let out = Command::new("nft")
        .arg("delete")
        .arg("table")
        .arg("inet")
        .arg(NFT_TABLE_NAME)
        .output()?;
    if !out.status.success() {
        // "No such file or directory" / "no such table" is expected when the
        // table was never installed; only surface other failures.
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.contains("No such file") && !stderr.contains("no such") {
            anyhow::bail!("nft delete table failed: {}", stderr);
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn apply_ruleset(
    _axon_port: u16,
    _v4: &BTreeSet<Ipv4Addr>,
    _v6: &BTreeSet<Ipv6Addr>,
) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn tear_down_table() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_ips_partitions_by_family() {
        let mut set = HashSet::new();
        set.insert("10.0.0.1".parse().unwrap());
        set.insert("2001:db8::1".parse().unwrap());
        let (v4, v6) = split_ips(&set);
        assert_eq!(v4.len(), 1);
        assert_eq!(v6.len(), 1);
    }
}
