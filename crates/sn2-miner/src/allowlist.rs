//! Trust-on-first-use validator allowlist.
//!
//! Bittensor validators on subnets that do not `serve_axon` (SN2 is one) do not
//! publish their source IPs to chain. This module learns the (validator hotkey
//! -> source IP) mapping from successful handshakes and gates source-IP
//! enforcement behind a stake-weighted coverage threshold so that operators
//! receive kernel-level packet drops without leaking validator IPs through the
//! chain.
//!
//! State machine:
//! * `Learning` -- source-IP enforcement disabled (handshake-time permit check
//!   still applied). Successful handshakes are added to the roster.
//! * `Enforcing` -- source-IP enforcement on; only roster IPs are admitted.
//!
//! Transitions are evaluated on every metagraph sync. The flip
//! `Learning -> Enforcing` requires both:
//! 1. one full epoch (`tempo` blocks) has elapsed since the process started,
//!    and
//! 2. `sum(stake of permit holders we have an IP for) / sum(stake of all permit
//!    holders) >= kappa`.
//!
//! The reverse `Enforcing -> Learning` fires whenever coverage drops below
//! kappa (validator rotation, hotkey churn, IP changes); the roster is
//! preserved so the next successful handshake re-covers without a fresh epoch
//! wait.

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result as AnyResult;
use btlightning::{
    HandshakeObserver, LightningError, Result as LightningResult, SourceAddressResolver,
    SourceAllowlist, ValidatorPermitResolver,
};
use sn2_chain::{Metagraph, NeuronInfo};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::roster::{self, Roster};

/// Mode flag indicating whether `roster::save` is exercised on observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    /// Load existing roster on startup and persist new observations.
    PersistToDisk,
    /// Skip the on-startup load and refuse to persist (volatile in-memory only).
    InMemoryOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Source-IP enforcement disabled while the roster fills.
    Learning,
    /// Source-IP enforcement active.
    Enforcing,
}

/// Snapshot returned by [`ValidatorAllowlist::evaluate`].
#[derive(Debug, Clone)]
pub struct Coverage {
    pub observed_permit_stake: u128,
    pub total_permit_stake: u128,
    pub kappa: u16,
    pub tempo: u16,
    pub blocks_since_start: u64,
    pub enforcing: bool,
    pub allowed_ips: HashSet<IpAddr>,
}

impl Coverage {
    pub fn fraction(&self) -> f64 {
        if self.total_permit_stake == 0 {
            0.0
        } else {
            self.observed_permit_stake as f64 / self.total_permit_stake as f64
        }
    }

    pub fn kappa_fraction(&self) -> f64 {
        self.kappa as f64 / u16::MAX as f64
    }
}

pub struct ValidatorAllowlist {
    metagraph: Arc<RwLock<Metagraph>>,
    roster: Arc<RwLock<Roster>>,
    state: Arc<RwLock<State>>,
    wallet_path: PathBuf,
    netuid: u16,
    cache_policy: CachePolicy,
    started_block: Arc<RwLock<Option<u64>>>,
}

impl ValidatorAllowlist {
    /// Builds the allowlist, optionally hydrating the roster from disk. The
    /// caller wires the metagraph that's already being kept in sync elsewhere;
    /// `evaluate` should be called immediately after each `Metagraph::sync` to
    /// recompute coverage and drive state transitions.
    pub fn new(
        metagraph: Arc<RwLock<Metagraph>>,
        netuid: u16,
        wallet_path: PathBuf,
        cache_policy: CachePolicy,
    ) -> AnyResult<Self> {
        let roster = match cache_policy {
            CachePolicy::PersistToDisk => roster::load(&wallet_path, netuid)?.unwrap_or_default(),
            CachePolicy::InMemoryOnly => Roster::new(),
        };
        if !roster.is_empty() {
            info!(
                entries = roster.len(),
                "loaded validator roster from disk; will require coverage re-verification before enforcing"
            );
        }
        Ok(Self {
            metagraph,
            roster: Arc::new(RwLock::new(roster)),
            state: Arc::new(RwLock::new(State::Learning)),
            wallet_path,
            netuid,
            cache_policy,
            started_block: Arc::new(RwLock::new(None)),
        })
    }

    /// Re-runs the state machine against the current metagraph + roster. Returns
    /// the coverage snapshot for the caller (used by the nftables manager and
    /// for logging).
    pub async fn evaluate(&self) -> Coverage {
        let meta = self.metagraph.read().await;
        let block = meta.block;
        let kappa = meta.kappa;
        let tempo = meta.tempo;

        // Anchor `started_block` to the first non-zero block we observe so the
        // tempo gate is robust across restarts within an epoch.
        let started_block = {
            let mut guard = self.started_block.write().await;
            *guard.get_or_insert(block)
        };
        let blocks_since_start = block.saturating_sub(started_block);

        let permit_iter = meta.neurons.iter().filter(|n| n.validator_permit);

        let mut total_permit_stake: u128 = 0;
        let mut observed_permit_stake: u128 = 0;
        let mut allowed_ips: HashSet<IpAddr> = HashSet::new();

        let roster = self.roster.read().await;
        for n in permit_iter {
            total_permit_stake = total_permit_stake.saturating_add(n.stake as u128);
            let mut covered = false;
            if let Some(chain_ip) = chain_axon_ip(n) {
                allowed_ips.insert(chain_ip);
                covered = true;
            }
            if let Some(roster_ip) = roster.ip_for(&n.hotkey) {
                allowed_ips.insert(roster_ip);
                covered = true;
            }
            if covered {
                observed_permit_stake = observed_permit_stake.saturating_add(n.stake as u128);
            }
        }
        drop(roster);
        drop(meta);

        let kappa_threshold = kappa as u128;
        let stake_meets_threshold = if total_permit_stake == 0 {
            false
        } else {
            // observed / total >= kappa / u16::MAX  <=>  observed * u16::MAX >= total * kappa
            observed_permit_stake.saturating_mul(u16::MAX as u128)
                >= total_permit_stake.saturating_mul(kappa_threshold)
        };
        let epoch_elapsed = tempo > 0 && blocks_since_start >= tempo as u64;

        let prev_state = *self.state.read().await;
        let next_state = match prev_state {
            State::Learning if epoch_elapsed && stake_meets_threshold => State::Enforcing,
            State::Enforcing if !stake_meets_threshold => State::Learning,
            other => other,
        };
        let state_changed = prev_state != next_state;
        if state_changed {
            *self.state.write().await = next_state;
            match next_state {
                State::Enforcing => info!(
                    observed_permit_stake,
                    total_permit_stake,
                    kappa,
                    tempo,
                    blocks_since_start,
                    "validator allowlist transition: Learning -> Enforcing (kappa coverage met)"
                ),
                State::Learning => warn!(
                    observed_permit_stake,
                    total_permit_stake,
                    kappa,
                    "validator allowlist transition: Enforcing -> Learning (coverage fell below kappa)"
                ),
            }
        }

        Coverage {
            observed_permit_stake,
            total_permit_stake,
            kappa,
            tempo,
            blocks_since_start,
            enforcing: matches!(next_state, State::Enforcing),
            allowed_ips,
        }
    }
}

impl ValidatorPermitResolver for ValidatorAllowlist {
    fn resolve_permitted_validators(&self) -> LightningResult<HashSet<String>> {
        let guard = self.metagraph.blocking_read();
        if guard.neurons.is_empty() {
            return Err(LightningError::Handler(
                "metagraph has not been synced; refusing to resolve empty permit set".to_string(),
            ));
        }
        Ok(guard
            .neurons
            .iter()
            .filter(|n| n.validator_permit)
            .map(|n| n.hotkey.clone())
            .collect())
    }
}

impl SourceAddressResolver for ValidatorAllowlist {
    fn resolve_allowed_sources(&self) -> LightningResult<SourceAllowlist> {
        let state = *self.state.blocking_read();
        match state {
            State::Learning => Ok(SourceAllowlist::Bypass),
            State::Enforcing => {
                let roster = self.roster.blocking_read();
                let meta = self.metagraph.blocking_read();
                let mut ips: HashSet<IpAddr> = HashSet::new();
                for n in meta.neurons.iter().filter(|n| n.validator_permit) {
                    if let Some(chain_ip) = chain_axon_ip(n) {
                        ips.insert(chain_ip);
                    }
                    if let Some(roster_ip) = roster.ip_for(&n.hotkey) {
                        ips.insert(roster_ip);
                    }
                }
                Ok(SourceAllowlist::Enforce(ips))
            }
        }
    }
}

/// Read `n.axon_ip` as an `IpAddr` if the chain entry is populated (non-empty
/// string, parseable, and not the all-zero sentinel). Validators that have not
/// run `serve_axon` keep the default `0.0.0.0` axon entry; treating that as
/// "no chain IP" lets the TOFU roster supply their IP instead.
fn chain_axon_ip(n: &NeuronInfo) -> Option<IpAddr> {
    if n.axon_ip.is_empty() {
        return None;
    }
    let ip: IpAddr = n.axon_ip.parse().ok()?;
    if ip.is_unspecified() {
        return None;
    }
    Some(ip)
}

#[async_trait::async_trait]
impl HandshakeObserver for ValidatorAllowlist {
    async fn observe_successful_handshake(&self, validator_hotkey: &str, source_ip: IpAddr) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let stake_snapshot = {
            let meta = self.metagraph.read().await;
            meta.neurons
                .iter()
                .find(|n| n.hotkey == validator_hotkey)
                .map(|n| n.stake)
                .unwrap_or(0)
        };

        let changed = {
            let mut roster = self.roster.write().await;
            roster.upsert(validator_hotkey, source_ip, now, stake_snapshot)
        };

        if changed && self.cache_policy == CachePolicy::PersistToDisk {
            let snapshot = self.roster.read().await.clone();
            let wallet_path = self.wallet_path.clone();
            let netuid = self.netuid;
            tokio::task::spawn_blocking(move || {
                if let Err(e) = roster::save(&wallet_path, netuid, &snapshot) {
                    warn!(error = %e, "failed to persist validator roster");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sn2_chain::NeuronInfo;

    fn neuron(uid: u16, hotkey: &str, stake: u64, permit: bool) -> NeuronInfo {
        NeuronInfo {
            uid,
            hotkey: hotkey.to_string(),
            coldkey: String::new(),
            hotkey_bytes: [0u8; 32],
            stake,
            rank: 0,
            trust: 0,
            consensus: 0,
            incentive: 0,
            dividends: 0,
            emission: 0,
            is_active: true,
            last_update: 0,
            axon_ip: String::new(),
            axon_port: 0,
            axon_protocol: 0,
            validator_permit: permit,
        }
    }

    fn build_meta(neurons: Vec<NeuronInfo>, block: u64, kappa: u16, tempo: u16) -> Metagraph {
        let mut m = Metagraph::from_neurons(2, neurons);
        m.block = block;
        m.kappa = kappa;
        m.tempo = tempo;
        m
    }

    async fn build_allowlist(meta: Metagraph) -> ValidatorAllowlist {
        let dir = tempfile::tempdir().unwrap();
        ValidatorAllowlist::new(
            Arc::new(RwLock::new(meta)),
            2,
            dir.path().to_path_buf(),
            CachePolicy::InMemoryOnly,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn learning_when_no_handshakes_yet() {
        let meta = build_meta(
            vec![
                neuron(0, "hk_big", 900, true),
                neuron(1, "hk_small", 100, true),
            ],
            500,
            32767,
            100,
        );
        let al = build_allowlist(meta).await;
        let cov = al.evaluate().await;
        assert!(!cov.enforcing);
        assert_eq!(cov.observed_permit_stake, 0);
        assert_eq!(cov.total_permit_stake, 1000);
    }

    #[tokio::test]
    async fn flips_to_enforcing_when_kappa_and_tempo_met() {
        let meta = build_meta(
            vec![
                neuron(0, "hk_big", 900, true),
                neuron(1, "hk_small", 100, true),
            ],
            500,
            32767,
            100,
        );
        let al = build_allowlist(meta).await;
        // First sync anchors `started_block` to 500.
        let _ = al.evaluate().await;
        // Observe the big validator: 900/1000 = 0.9 > 0.5 kappa.
        al.observe_successful_handshake("hk_big", "10.0.0.1".parse().unwrap())
            .await;
        // Advance the chain by `tempo` blocks: 500 + 100 = 600.
        al.metagraph.write().await.block = 600;
        let cov = al.evaluate().await;
        assert!(cov.enforcing, "should enforce after kappa+tempo met");
        assert_eq!(cov.allowed_ips.len(), 1);
    }

    #[tokio::test]
    async fn holds_in_learning_while_tempo_not_met() {
        let meta = build_meta(vec![neuron(0, "hk_big", 1000, true)], 500, 32767, 100);
        let al = build_allowlist(meta).await;
        let _ = al.evaluate().await;
        al.observe_successful_handshake("hk_big", "10.0.0.1".parse().unwrap())
            .await;
        // Only 50 blocks elapse, tempo is 100.
        al.metagraph.write().await.block = 550;
        let cov = al.evaluate().await;
        assert!(!cov.enforcing, "should not enforce before one epoch");
    }

    #[tokio::test]
    async fn falls_back_to_learning_when_coverage_drops() {
        let meta = build_meta(
            vec![
                neuron(0, "hk_big", 900, true),
                neuron(1, "hk_small", 100, true),
            ],
            500,
            32767,
            100,
        );
        let al = build_allowlist(meta).await;
        let _ = al.evaluate().await;
        al.observe_successful_handshake("hk_big", "10.0.0.1".parse().unwrap())
            .await;
        al.metagraph.write().await.block = 600;
        let cov = al.evaluate().await;
        assert!(cov.enforcing);

        // Simulate a metagraph rotation: hk_big loses its permit, hk_new takes its place.
        {
            let mut meta = al.metagraph.write().await;
            meta.neurons = vec![
                neuron(0, "hk_big", 900, false),
                neuron(1, "hk_small", 100, true),
                neuron(2, "hk_new", 900, true),
            ];
            meta.block = 700;
        }
        let cov = al.evaluate().await;
        assert!(
            !cov.enforcing,
            "coverage drops to 100/1000 = 10% < kappa; must fall back to Learning"
        );
    }

    fn neuron_with_axon(uid: u16, hotkey: &str, stake: u64, axon_ip: &str) -> NeuronInfo {
        let mut n = neuron(uid, hotkey, stake, true);
        n.axon_ip = axon_ip.to_string();
        n
    }

    #[tokio::test]
    async fn chain_axon_ip_covers_validator_without_handshake() {
        // hk_chain publishes an axon IP on chain; hk_legacy does not.
        // The chain entry alone should make hk_chain's stake count toward
        // kappa coverage and place its IP in the enforced source set.
        let meta = build_meta(
            vec![
                neuron_with_axon(0, "hk_chain", 900, "10.0.0.1"),
                neuron(1, "hk_legacy", 100, true),
            ],
            500,
            32767,
            100,
        );
        let al = build_allowlist(meta).await;
        let _ = al.evaluate().await;
        // No handshake observed for hk_chain — coverage must come from the chain entry.
        al.metagraph.write().await.block = 600;
        let cov = al.evaluate().await;
        assert!(
            cov.enforcing,
            "chain axon IP alone should satisfy kappa coverage"
        );
        assert!(cov.allowed_ips.contains(&"10.0.0.1".parse().unwrap()));
    }

    #[tokio::test]
    async fn chain_unspecified_ip_does_not_count_as_coverage() {
        // Validators that have not run serve_axon keep 0.0.0.0 on chain; this
        // must be treated identically to an absent entry.
        let meta = build_meta(
            vec![
                neuron_with_axon(0, "hk_unset", 900, "0.0.0.0"),
                neuron(1, "hk_other", 100, true),
            ],
            500,
            32767,
            100,
        );
        let al = build_allowlist(meta).await;
        let _ = al.evaluate().await;
        al.metagraph.write().await.block = 600;
        let cov = al.evaluate().await;
        assert!(
            !cov.enforcing,
            "0.0.0.0 axon entry must not count toward coverage"
        );
    }

    #[tokio::test]
    async fn chain_and_roster_ips_are_unioned() {
        // hk_chain publishes one IP on chain; the roster has observed it from
        // a different address (e.g., the validator just rotated egress). Both
        // IPs should appear in the enforced source set so the rotation does
        // not interrupt traffic.
        let meta = build_meta(
            vec![neuron_with_axon(0, "hk_chain", 1000, "10.0.0.1")],
            500,
            32767,
            100,
        );
        let al = build_allowlist(meta).await;
        let _ = al.evaluate().await;
        al.observe_successful_handshake("hk_chain", "10.0.0.2".parse().unwrap())
            .await;
        al.metagraph.write().await.block = 600;
        let cov = al.evaluate().await;
        assert!(cov.enforcing);
        assert!(cov.allowed_ips.contains(&"10.0.0.1".parse().unwrap()));
        assert!(cov.allowed_ips.contains(&"10.0.0.2".parse().unwrap()));
    }
}
