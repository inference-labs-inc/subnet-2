use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use rand::{rngs::StdRng, Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::performance::{CapDirection, PerformanceTracker};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub run: RunConfig,
    pub miners: Vec<MinerProfile>,
    #[serde(default)]
    pub events: Vec<ScheduledEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunConfig {
    pub duration_secs: u64,
    pub tick_interval_ms: u64,
    pub verification_concurrency: usize,
    pub sample_rate: f64,
    pub sample_verify_ms: u64,
    pub output_path: String,
    pub health_every_secs: u64,
    pub rng_seed: u64,
    pub block_time_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinerProfile {
    pub uid: u16,
    pub hotkey: String,
    pub response_time_ms: u64,
    pub success_rate: f64,
    pub cap_ceiling: usize,
    pub alive_after_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledEvent {
    pub at_secs: u64,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventKind {
    SetResponseTime { uid: u16, value_ms: u64 },
    SetSuccessRate { uid: u16, value: f64 },
    KillMiner { uid: u16 },
    AddMiner(MinerProfile),
}

#[derive(Debug, Eq, PartialEq)]
enum SimEventKind {
    NetworkResponse {
        uid: u16,
        hotkey: String,
        success: bool,
        response_time_ms: u64,
        was_at_capacity: bool,
    },
    VerifyCompletion {
        uid: u16,
        hotkey: String,
        response_time_ms: u64,
        was_at_capacity: bool,
    },
}

#[derive(Debug, Eq, PartialEq)]
struct SimEvent {
    due_at_ms: u64,
    seq: u64,
    kind: SimEventKind,
}

impl Ord for SimEvent {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.due_at_ms
            .cmp(&other.due_at_ms)
            .then_with(|| self.seq.cmp(&other.seq))
    }
}

impl PartialOrd for SimEvent {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

struct MinerRuntime {
    profile: MinerProfile,
    active: usize,
    dispatched_total: u64,
    succeeded_total: u64,
    failed_total: u64,
}

struct SimState {
    sim_time_ms: u64,
    current_block: u64,
    tracker: PerformanceTracker,
    dispatch_cooldowns: HashMap<String, u64>,
    miners: HashMap<u16, MinerRuntime>,
    uid_hotkeys: HashMap<u16, String>,
    events: BinaryHeap<Reverse<SimEvent>>,
    pending_verifications: VecDeque<(u16, String, u64, bool)>,
    verify_in_flight: usize,
    verification_concurrency: usize,
    sample_rate: f64,
    sample_verify_ms: u64,
    block_time_ms: u64,
    dispatch_halt_count: u64,
    rng: StdRng,
    seq: u64,
}

impl SimState {
    fn new(scenario: &Scenario) -> Self {
        let mut miners = HashMap::new();
        let mut uid_hotkeys = HashMap::new();
        for m in &scenario.miners {
            uid_hotkeys.insert(m.uid, m.hotkey.clone());
            miners.insert(
                m.uid,
                MinerRuntime {
                    profile: m.clone(),
                    active: 0,
                    dispatched_total: 0,
                    succeeded_total: 0,
                    failed_total: 0,
                },
            );
        }
        Self {
            sim_time_ms: 0,
            current_block: 0,
            tracker: PerformanceTracker::new(),
            dispatch_cooldowns: HashMap::new(),
            miners,
            uid_hotkeys,
            events: BinaryHeap::new(),
            pending_verifications: VecDeque::new(),
            verify_in_flight: 0,
            verification_concurrency: scenario.run.verification_concurrency,
            sample_rate: scenario.run.sample_rate,
            sample_verify_ms: scenario.run.sample_verify_ms,
            block_time_ms: scenario.run.block_time_ms,
            dispatch_halt_count: 0,
            rng: StdRng::seed_from_u64(scenario.run.rng_seed),
            seq: 0,
        }
    }

    fn push_event(&mut self, due_at_ms: u64, kind: SimEventKind) {
        self.seq += 1;
        self.events.push(Reverse(SimEvent {
            due_at_ms,
            seq: self.seq,
            kind,
        }));
    }

    fn apply_scheduled_events(&mut self, scheduled: &mut Vec<ScheduledEvent>) {
        let now_secs = self.sim_time_ms / 1000;
        let mut idx = 0;
        while idx < scheduled.len() {
            if scheduled[idx].at_secs > now_secs {
                idx += 1;
                continue;
            }
            let evt = scheduled.remove(idx);
            match evt.kind {
                EventKind::SetResponseTime { uid, value_ms } => {
                    if let Some(m) = self.miners.get_mut(&uid) {
                        info!(
                            t_secs = now_secs,
                            uid,
                            old_ms = m.profile.response_time_ms,
                            new_ms = value_ms,
                            "scenario: set_response_time"
                        );
                        m.profile.response_time_ms = value_ms;
                    }
                }
                EventKind::SetSuccessRate { uid, value } => {
                    if let Some(m) = self.miners.get_mut(&uid) {
                        info!(t_secs = now_secs, uid, old = m.profile.success_rate, new = value, "scenario: set_success_rate");
                        m.profile.success_rate = value;
                    }
                }
                EventKind::KillMiner { uid } => {
                    info!(t_secs = now_secs, uid, "scenario: kill_miner");
                    self.miners.remove(&uid);
                }
                EventKind::AddMiner(profile) => {
                    info!(t_secs = now_secs, uid = profile.uid, hotkey = %profile.hotkey, "scenario: add_miner");
                    self.uid_hotkeys.insert(profile.uid, profile.hotkey.clone());
                    self.miners.insert(
                        profile.uid,
                        MinerRuntime {
                            profile,
                            active: 0,
                            dispatched_total: 0,
                            succeeded_total: 0,
                            failed_total: 0,
                        },
                    );
                }
            }
        }
    }

    fn process_due_events(&mut self) {
        while let Some(Reverse(evt)) = self.events.peek() {
            if evt.due_at_ms > self.sim_time_ms {
                break;
            }
            let Reverse(evt) = self.events.pop().unwrap();
            match evt.kind {
                SimEventKind::NetworkResponse {
                    uid,
                    hotkey,
                    success,
                    response_time_ms,
                    was_at_capacity,
                } => {
                    if let Some(m) = self.miners.get_mut(&uid) {
                        m.active = m.active.saturating_sub(1);
                        if success {
                            m.succeeded_total += 1;
                        } else {
                            m.failed_total += 1;
                        }
                    }
                    let sample = self.rng.random::<f64>() < self.sample_rate;
                    if !sample {
                        self.record_finish(
                            uid,
                            &hotkey,
                            success,
                            response_time_ms,
                            was_at_capacity,
                        );
                    } else if self.verify_in_flight < self.verification_concurrency {
                        self.verify_in_flight += 1;
                        let due = self.sim_time_ms + self.sample_verify_ms;
                        self.push_event(
                            due,
                            SimEventKind::VerifyCompletion {
                                uid,
                                hotkey,
                                response_time_ms,
                                was_at_capacity,
                            },
                        );
                    } else {
                        self.pending_verifications.push_back((
                            uid,
                            hotkey,
                            response_time_ms,
                            was_at_capacity,
                        ));
                    }
                }
                SimEventKind::VerifyCompletion {
                    uid,
                    hotkey,
                    response_time_ms,
                    was_at_capacity,
                } => {
                    self.verify_in_flight = self.verify_in_flight.saturating_sub(1);
                    self.record_finish(uid, &hotkey, true, response_time_ms, was_at_capacity);
                    while self.verify_in_flight < self.verification_concurrency {
                        let Some((u, h, rt, wac)) = self.pending_verifications.pop_front() else {
                            break;
                        };
                        self.verify_in_flight += 1;
                        let due = self.sim_time_ms + self.sample_verify_ms;
                        self.push_event(
                            due,
                            SimEventKind::VerifyCompletion {
                                uid: u,
                                hotkey: h,
                                response_time_ms: rt,
                                was_at_capacity: wac,
                            },
                        );
                    }
                }
            }
        }
    }

    fn record_finish(
        &mut self,
        uid: u16,
        hotkey: &str,
        success: bool,
        response_time_ms: u64,
        was_at_capacity: bool,
    ) {
        let now = Instant::now();
        self.tracker.record_with_time(
            uid,
            hotkey,
            success,
            response_time_ms as f64 / 1000.0,
            was_at_capacity,
            now,
        );
    }

    fn absorb_evictions(&mut self) {
        let evicted = self.tracker.drain_pending_evictions();
        if evicted.is_empty() {
            return;
        }
        let until = self.current_block.saturating_add(sn2_types::REHAB_BLOCKS);
        for (uid, hotkey) in evicted {
            let prev = self.dispatch_cooldowns.get(&hotkey).copied().unwrap_or(0);
            let new_until = std::cmp::max(prev, until);
            self.dispatch_cooldowns.insert(hotkey.clone(), new_until);
            if prev == 0 {
                info!(
                    t_secs = self.sim_time_ms / 1000,
                    uid,
                    until_block = new_until,
                    "evicted from dispatch"
                );
            }
        }
    }

    fn dispatch(&mut self) {
        let pending_cap = self.verification_concurrency.saturating_mul(4);
        if self.pending_verifications.len() >= pending_cap {
            self.dispatch_halt_count += 1;
            return;
        }
        let alive_after_ok = |m: &MinerRuntime| (m.profile.alive_after_secs * 1000) <= self.sim_time_ms;
        let now_block = self.current_block;
        let queryable: Vec<u16> = self
            .miners
            .iter()
            .filter(|(_, m)| alive_after_ok(m))
            .filter(|(_, m)| {
                if let Some(&until) = self.dispatch_cooldowns.get(&m.profile.hotkey) {
                    if now_block < until {
                        return false;
                    }
                }
                true
            })
            .map(|(uid, _)| *uid)
            .collect();
        let caps = self.tracker.miner_capacities();
        for uid in queryable {
            let cap = caps.get(&uid).copied().unwrap_or(1);
            let active = self.miners.get(&uid).map(|m| m.active).unwrap_or(0);
            if active >= cap {
                continue;
            }
            let slots = cap - active;
            for _ in 0..slots {
                let m = match self.miners.get_mut(&uid) {
                    Some(m) => m,
                    None => break,
                };
                let was_at_capacity = (m.active + 1) >= cap;
                let success = self.rng.random::<f64>() < m.profile.success_rate
                    && m.active < m.profile.cap_ceiling;
                let rt = m.profile.response_time_ms;
                let hotkey = m.profile.hotkey.clone();
                m.active += 1;
                m.dispatched_total += 1;
                let due = self.sim_time_ms + rt;
                self.push_event(
                    due,
                    SimEventKind::NetworkResponse {
                        uid,
                        hotkey,
                        success,
                        response_time_ms: rt,
                        was_at_capacity,
                    },
                );
            }
        }
    }

    fn maintenance_tick(&mut self) {
        let cooldowns_before = self.dispatch_cooldowns.len();
        let expired: Vec<String> = self
            .dispatch_cooldowns
            .iter()
            .filter(|(_, &until)| self.current_block >= until)
            .map(|(hk, _)| hk.clone())
            .collect();
        for hk in &expired {
            self.dispatch_cooldowns.remove(hk);
            if let Some(&uid) = self
                .uid_hotkeys
                .iter()
                .find(|(_, h)| *h == hk)
                .map(|(u, _)| u)
            {
                self.tracker.rehabilitate(uid, hk);
                info!(
                    t_secs = self.sim_time_ms / 1000,
                    uid, "cooldown expired, rehabilitated"
                );
            }
        }
        let cooldowns_after = self.dispatch_cooldowns.len();
        if cooldowns_before != cooldowns_after {
            info!(
                t_secs = self.sim_time_ms / 1000,
                expired = cooldowns_before - cooldowns_after,
                "cooldowns pruned"
            );
        }
        self.tracker.decay_idle_caps(&self.uid_hotkeys);
    }

    fn emit_health(&self, output: &mut impl Write) -> std::io::Result<()> {
        let caps = self.tracker.miner_capacities();
        let t_secs = self.sim_time_ms / 1000;
        for (uid, m) in self.miners.iter() {
            let cap = caps.get(uid).copied().unwrap_or(1);
            let in_cooldown = self
                .dispatch_cooldowns
                .get(&m.profile.hotkey)
                .map(|&until| self.current_block < until)
                .unwrap_or(false);
            writeln!(
                output,
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                t_secs,
                uid,
                cap,
                m.active,
                m.dispatched_total,
                m.succeeded_total,
                m.failed_total,
                if in_cooldown { 1 } else { 0 },
                self.pending_verifications.len(),
                self.verify_in_flight,
                self.dispatch_halt_count,
                m.profile.response_time_ms,
                m.profile.success_rate,
                m.profile.cap_ceiling,
                m.profile.alive_after_secs,
            )?;
        }
        output.flush()?;
        Ok(())
    }
}

pub fn builtin_scenario(name: &str) -> Option<Scenario> {
    match name {
        "new-miner" => Some(scenario_new_miner_discovery()),
        "old-miner-decay" => Some(scenario_old_miner_decay()),
        "overload" => Some(scenario_overload()),
        _ => None,
    }
}

fn baseline_run(output_suffix: &str) -> RunConfig {
    RunConfig {
        duration_secs: 1800,
        tick_interval_ms: 50,
        verification_concurrency: 128,
        sample_rate: 0.04,
        sample_verify_ms: 200,
        output_path: format!("/tmp/sim_{output_suffix}.tsv"),
        health_every_secs: 15,
        rng_seed: 42,
        block_time_ms: 12_000,
    }
}

fn scenario_new_miner_discovery() -> Scenario {
    Scenario {
        run: baseline_run("new_miner"),
        miners: vec![
            MinerProfile {
                uid: 1,
                hotkey: "hk_incumbent_a".into(),
                response_time_ms: 500,
                success_rate: 0.95,
                cap_ceiling: 100,
                alive_after_secs: 0,
            },
            MinerProfile {
                uid: 2,
                hotkey: "hk_incumbent_b".into(),
                response_time_ms: 800,
                success_rate: 0.92,
                cap_ceiling: 80,
                alive_after_secs: 0,
            },
        ],
        events: vec![ScheduledEvent {
            at_secs: 600,
            kind: EventKind::AddMiner(MinerProfile {
                uid: 99,
                hotkey: "hk_new_fast".into(),
                response_time_ms: 100,
                success_rate: 0.99,
                cap_ceiling: 500,
                alive_after_secs: 600,
            }),
        }],
    }
}

fn scenario_old_miner_decay() -> Scenario {
    Scenario {
        run: baseline_run("old_miner_decay"),
        miners: vec![
            MinerProfile {
                uid: 10,
                hotkey: "hk_steady".into(),
                response_time_ms: 400,
                success_rate: 0.97,
                cap_ceiling: 200,
                alive_after_secs: 0,
            },
            MinerProfile {
                uid: 20,
                hotkey: "hk_degrading".into(),
                response_time_ms: 300,
                success_rate: 0.99,
                cap_ceiling: 1000,
                alive_after_secs: 0,
            },
        ],
        events: vec![
            ScheduledEvent {
                at_secs: 600,
                kind: EventKind::SetResponseTime {
                    uid: 20,
                    value_ms: 30_000,
                },
            },
            ScheduledEvent {
                at_secs: 600,
                kind: EventKind::SetSuccessRate {
                    uid: 20,
                    value: 0.40,
                },
            },
        ],
    }
}

fn scenario_overload() -> Scenario {
    let mut miners = Vec::new();
    for i in 0..200u16 {
        miners.push(MinerProfile {
            uid: i,
            hotkey: format!("hk_{i:03}"),
            response_time_ms: 50,
            success_rate: 0.98,
            cap_ceiling: 5000,
            alive_after_secs: 0,
        });
    }
    Scenario {
        run: baseline_run("overload"),
        miners,
        events: Vec::new(),
    }
}

pub async fn run(input: &str) -> Result<()> {
    let scenario = if let Some(s) = builtin_scenario(input) {
        s
    } else {
        let path = Path::new(input);
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading scenario file {}", path.display()))?;
        rmp_serde::from_slice(&bytes).context("decoding msgpack scenario")?
    };
    run_scenario(scenario)
}

fn run_scenario(scenario: Scenario) -> Result<()> {
    info!(
        miners = scenario.miners.len(),
        duration_secs = scenario.run.duration_secs,
        tick_ms = scenario.run.tick_interval_ms,
        verification_concurrency = scenario.run.verification_concurrency,
        sample_rate = scenario.run.sample_rate,
        output = %scenario.run.output_path,
        "simulator starting"
    );
    let mut state = SimState::new(&scenario);
    let mut output = std::fs::File::create(&scenario.run.output_path)
        .with_context(|| format!("creating {}", scenario.run.output_path))?;
    writeln!(
        output,
        "time_secs\tuid\tcap\tactive\tdispatched\tsucceeded\tfailed\tin_cooldown\tpending_verify\tverify_in_flight\tdispatch_halts\trt_ms\tsuccess_rate\tcap_ceiling\talive_after_secs"
    )?;

    let mut pending_events = scenario.events.clone();
    pending_events.sort_by_key(|e| e.at_secs);

    let total_ms = scenario.run.duration_secs * 1000;
    let tick_ms = scenario.run.tick_interval_ms;
    let health_every_ms = scenario.run.health_every_secs * 1000;
    let mut next_health_ms = 0;
    let mut next_maintenance_ms: u64 = 60_000;

    while state.sim_time_ms <= total_ms {
        state.apply_scheduled_events(&mut pending_events);
        state.process_due_events();
        state.absorb_evictions();
        state.dispatch();
        if state.sim_time_ms >= next_maintenance_ms {
            state.maintenance_tick();
            next_maintenance_ms += 60_000;
        }
        if state.sim_time_ms >= next_health_ms {
            state.emit_health(&mut output)?;
            next_health_ms += health_every_ms;
        }
        state.sim_time_ms += tick_ms;
        state.current_block = state.sim_time_ms / state.block_time_ms.max(1);
    }

    summarize(&state, &scenario);
    info!("simulator complete");
    Ok(())
}

fn summarize(state: &SimState, scenario: &Scenario) {
    let caps = state.tracker.miner_capacities();
    let mut rows: Vec<_> = state.miners.values().collect();
    rows.sort_by_key(|m| m.profile.uid);
    info!(
        events_remaining = state.events.len(),
        pending_verify = state.pending_verifications.len(),
        verify_in_flight = state.verify_in_flight,
        dispatch_halts = state.dispatch_halt_count,
        "final state"
    );
    for m in rows {
        let cap = caps.get(&m.profile.uid).copied().unwrap_or(1);
        let in_cooldown = state
            .dispatch_cooldowns
            .get(&m.profile.hotkey)
            .map(|&until| state.current_block < until)
            .unwrap_or(false);
        info!(
            uid = m.profile.uid,
            hotkey = %m.profile.hotkey,
            cap,
            in_cooldown,
            dispatched = m.dispatched_total,
            succeeded = m.succeeded_total,
            failed = m.failed_total,
            rt_ms = m.profile.response_time_ms,
            success_rate = m.profile.success_rate,
            cap_ceiling = m.profile.cap_ceiling,
            "miner summary"
        );
    }
    let _ = scenario;
}

impl Drop for SimState {
    fn drop(&mut self) {
        let _ = self.tracker.drain_cap_events();
    }
}

#[allow(dead_code)]
fn cap_dir_to_str(d: CapDirection) -> &'static str {
    match d {
        CapDirection::Ramp => "ramp",
        CapDirection::Backoff => "backoff",
        CapDirection::Evict => "evict",
        CapDirection::Rehab => "rehab",
    }
}
