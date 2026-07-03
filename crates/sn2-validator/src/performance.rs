use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sn2_types::{
    ADAPTIVE_TIMEOUT_MIN_SAMPLES, ADAPTIVE_TIMEOUT_MULTIPLIER, ADAPTIVE_TIMEOUT_PERCENTILE,
    BLOCK_TIME_SECS, CAPACITY_BACKOFF_THRESHOLD, CAPACITY_MIN_AT_CAP,
    CAPACITY_RAMP_MIN_AVAIL_MEM_RATIO, CAPACITY_RAMP_THRESHOLD,
    CAPACITY_SATURATION_LATENCY_FLOOR_SECS, CAPACITY_SATURATION_RAMP_CEILING,
    CAPACITY_SATURATION_TOLERANCE, CAPACITY_UNIT_REFERENCE_PERCENTILE, CAPACITY_WINDOW_SIZE,
    CIRCUIT_TIMEOUT_SECONDS, DELIVERED_WORK_BUCKET_SECS, FAILURE_DEBIT_MULTIPLIER,
    PERFORMANCE_MIN_SAMPLES, PERFORMANCE_RESCHEDULE_PENALTY, PERFORMANCE_WINDOW_SIZE,
    VERIFICATION_WINDOW_BLOCKS,
};
use tracing::{debug, warn};

#[cfg(target_os = "linux")]
pub(crate) fn host_memory_available_ratio() -> Option<f64> {
    let raw = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo_avail_ratio(&raw)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn host_memory_available_ratio() -> Option<f64> {
    None
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_meminfo_avail_ratio(raw: &str) -> Option<f64> {
    let mut total_kib: Option<u64> = None;
    let mut avail_kib: Option<u64> = None;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kib = rest.split_whitespace().next().and_then(|n| n.parse().ok());
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail_kib = rest.split_whitespace().next().and_then(|n| n.parse().ok());
        }
    }
    match (total_kib, avail_kib) {
        (Some(t), Some(a)) if t > 0 => Some(a as f64 / t as f64),
        _ => None,
    }
}

pub(crate) fn cap_ramp_blocked_by_memory_pressure() -> bool {
    host_memory_available_ratio()
        .map(|r| r < CAPACITY_RAMP_MIN_AVAIL_MEM_RATIO)
        .unwrap_or(false)
}

type WindowEntry = (Instant, bool, f64);

const MAX_BUFFERED_CAP_EVENTS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapDirection {
    Ramp,
    Backoff,
    Evict,
    Rehab,
}

#[derive(Clone, Debug)]
pub struct CapEvent {
    pub uid: u16,
    pub hotkey: String,
    pub direction: CapDirection,
    pub cap_from: usize,
    pub cap_to: usize,
    pub success_rate: f64,
    pub at: Instant,
}

pub struct PerformanceTracker {
    windows: HashMap<u16, VecDeque<WindowEntry>>,
    adaptive_caps: HashMap<u16, usize>,
    at_cap_results: HashMap<u16, VecDeque<bool>>,
    cap_events: Vec<CapEvent>,
    pending_evictions: Vec<(u16, String)>,
    at_cap_last_touched: HashMap<u16, Instant>,
    unit_times: HashMap<String, VecDeque<(Instant, f64)>>,
    unit_reference_cache: HashMap<String, (u64, f64)>,
    rel_speed: HashMap<u16, VecDeque<(Instant, f64)>>,
    unit_own_best: HashMap<u16, HashMap<String, (Instant, f64)>>,
    miner_slowdown: HashMap<u16, VecDeque<(Instant, f64)>>,
    delivered_work: HashMap<u16, VecDeque<(Instant, WorkBucket)>>,
    total_records: u64,
    persistence_path: Option<PathBuf>,
}

const CAP_DECAY_IDLE_SECS: u64 = 600;

const UNIT_REFERENCE_REFRESH_RECORDS: u64 = 64;
const UNIT_TIMES_CAP: usize = 256;
const UNIT_REFERENCE_MIN_SAMPLES: usize = 8;
const REL_SPEED_WINDOW: usize = 64;

const SLOWDOWN_WINDOW: usize = 64;

const RESTORED_CAP_MAX: usize = 32;

fn window_ttl() -> Duration {
    Duration::from_secs(VERIFICATION_WINDOW_BLOCKS * BLOCK_TIME_SECS)
}

fn evict_expired(window: &mut VecDeque<WindowEntry>) {
    let ttl = window_ttl();
    while let Some((ts, _, _)) = window.front() {
        if ts.elapsed() > ttl {
            window.pop_front();
        } else {
            break;
        }
    }
    while window.len() > PERFORMANCE_WINDOW_SIZE {
        window.pop_front();
    }
}

#[derive(Default, Clone, Copy)]
pub struct WorkBucket {
    pub credit: f64,
    pub debit: f64,
    pub uncredited_failures: u64,
}

fn push_bucketed(
    buckets: &mut VecDeque<(Instant, WorkBucket)>,
    now: Instant,
    credit: f64,
    debit: f64,
    uncredited: u64,
) {
    match buckets.back_mut() {
        Some((start, bucket))
            if now.duration_since(*start).as_secs() < DELIVERED_WORK_BUCKET_SECS =>
        {
            bucket.credit += credit;
            bucket.debit += debit;
            bucket.uncredited_failures += uncredited;
        }
        _ => buckets.push_back((
            now,
            WorkBucket {
                credit,
                debit,
                uncredited_failures: uncredited,
            },
        )),
    }
    let ttl = window_ttl() + Duration::from_secs(DELIVERED_WORK_BUCKET_SECS);
    while let Some((start, _)) = buckets.front() {
        if now.duration_since(*start) > ttl {
            buckets.pop_front();
        } else {
            break;
        }
    }
}

fn evict_timed(samples: &mut VecDeque<(Instant, f64)>, cap: usize) {
    let ttl = window_ttl();
    while let Some((ts, _)) = samples.front() {
        if ts.elapsed() > ttl {
            samples.pop_front();
        } else {
            break;
        }
    }
    while samples.len() > cap {
        samples.pop_front();
    }
}

impl PerformanceTracker {
    pub fn new_with_persistence(path: PathBuf) -> Self {
        let mut tracker = Self {
            windows: HashMap::new(),
            adaptive_caps: HashMap::new(),
            at_cap_results: HashMap::new(),
            cap_events: Vec::new(),
            pending_evictions: Vec::new(),
            at_cap_last_touched: HashMap::new(),
            unit_times: HashMap::new(),
            unit_reference_cache: HashMap::new(),
            rel_speed: HashMap::new(),
            unit_own_best: HashMap::new(),
            miner_slowdown: HashMap::new(),
            delivered_work: HashMap::new(),
            total_records: 0,
            persistence_path: Some(path),
        };
        tracker.load();
        tracker
    }

    #[cfg(test)]
    pub fn record(
        &mut self,
        uid: u16,
        hotkey: &str,
        success: bool,
        response_time: f64,
        was_at_capacity: bool,
    ) {
        self.record_keyed(uid, hotkey, success, response_time, was_at_capacity, "");
    }

    pub fn record_keyed(
        &mut self,
        uid: u16,
        hotkey: &str,
        success: bool,
        response_time: f64,
        was_at_capacity: bool,
        work_key: &str,
    ) {
        self.record_with_time(
            uid,
            hotkey,
            success,
            response_time,
            was_at_capacity,
            work_key,
            Instant::now(),
        );
    }

    pub fn record_with_time(
        &mut self,
        uid: u16,
        hotkey: &str,
        success: bool,
        response_time: f64,
        was_at_capacity: bool,
        work_key: &str,
        now: Instant,
    ) {
        self.total_records = self.total_records.wrapping_add(1);
        let window = self.windows.entry(uid).or_default();
        window.push_back((now, success, response_time));
        evict_expired(window);

        if success && response_time > 0.0 {
            let unit = self.unit_times.entry(work_key.to_string()).or_default();
            unit.push_back((now, response_time));
            evict_timed(unit, UNIT_TIMES_CAP);

            // The miner's own best latency on this unit, held for a full window
            // so sustained load can't drag the baseline up with it. It only
            // resets upward when no faster sample has been seen for the whole
            // TTL (a genuine, lasting slowdown), not when we are congesting it.
            let best_entry = self
                .unit_own_best
                .entry(uid)
                .or_default()
                .entry(work_key.to_string())
                .or_insert((now, response_time));
            if response_time <= best_entry.1 || now.duration_since(best_entry.0) > window_ttl() {
                *best_entry = (now, response_time);
            }
            let own_best = best_entry.1;

            if own_best > 0.0 {
                // Saturation: how far this miner's current latency has drifted
                // above its own best. Drives the cap, self-referential.
                let slowdown = (response_time.max(CAPACITY_SATURATION_LATENCY_FLOOR_SECS)
                    / own_best.max(CAPACITY_SATURATION_LATENCY_FLOOR_SECS))
                .max(1.0);
                let trend = self.miner_slowdown.entry(uid).or_default();
                trend.push_back((now, slowdown));
                evict_timed(trend, SLOWDOWN_WINDOW);

                // Weight rate: the circuit's fast end (load-independent) over
                // this miner's own best. Both ends ignore queue latency, so the
                // ratio reflects intrinsic speed and cannot be inflated by load.
                let reference = self.cached_unit_reference(work_key);
                if reference > 0.0 {
                    let rel = reference / own_best;
                    let samples = self.rel_speed.entry(uid).or_default();
                    samples.push_back((now, rel));
                    evict_timed(samples, REL_SPEED_WINDOW);

                    push_bucketed(
                        self.delivered_work.entry(uid).or_default(),
                        now,
                        reference,
                        0.0,
                        0,
                    );
                }
            }
        }

        if was_at_capacity {
            let results = self.at_cap_results.entry(uid).or_default();
            results.push_back(success);
            if results.len() > CAPACITY_WINDOW_SIZE {
                results.pop_front();
            }
            self.at_cap_last_touched.insert(uid, now);
            self.update_adaptive_cap(uid, hotkey);
        }
    }

    #[cfg(test)]
    pub fn record_reschedule(&mut self, uid: u16) {
        self.record_reschedule_keyed(uid, "");
    }

    pub fn record_reschedule_keyed(&mut self, uid: u16, work_key: &str) {
        let now = Instant::now();
        let window = self.windows.entry(uid).or_default();
        window.push_back((now, false, PERFORMANCE_RESCHEDULE_PENALTY));
        evict_expired(window);

        let reference = if work_key.is_empty() {
            0.0
        } else {
            self.cached_unit_reference(work_key)
        };
        if reference > 0.0 {
            push_bucketed(
                self.delivered_work.entry(uid).or_default(),
                now,
                0.0,
                reference * FAILURE_DEBIT_MULTIPLIER,
                0,
            );
        } else {
            push_bucketed(
                self.delivered_work.entry(uid).or_default(),
                now,
                0.0,
                0.0,
                1,
            );
        }
    }

    pub fn adaptive_timeout(&self) -> f64 {
        let times: Vec<f64> = self
            .windows
            .values()
            .flat_map(|w| w.iter().filter(|(_, s, _)| *s).map(|(_, _, t)| *t))
            .collect();

        if times.len() < ADAPTIVE_TIMEOUT_MIN_SAMPLES {
            return CIRCUIT_TIMEOUT_SECONDS as f64;
        }

        let mut sorted = times;
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let idx = ((sorted.len() as f64 * ADAPTIVE_TIMEOUT_PERCENTILE) as usize)
            .min(sorted.len().saturating_sub(1));
        let p95 = sorted[idx];

        (p95 * ADAPTIVE_TIMEOUT_MULTIPLIER).min(CIRCUIT_TIMEOUT_SECONDS as f64)
    }

    pub fn miner_capacities(&self) -> HashMap<u16, usize> {
        self.windows
            .iter()
            .map(|(&uid, w)| {
                if w.len() < PERFORMANCE_MIN_SAMPLES {
                    (uid, 1)
                } else {
                    (uid, self.adaptive_caps.get(&uid).copied().unwrap_or(1))
                }
            })
            .collect()
    }

    pub fn evict_all_stale(&mut self) {
        for w in self.windows.values_mut() {
            evict_expired(w);
        }
        self.windows.retain(|_, w| !w.is_empty());
    }

    /// Globally trim every adaptive cap by `factor` (rounded to at least 1
    /// decrement) when the validator host is under memory pressure. Returns
    /// the number of caps that actually decreased, for monitoring.
    pub fn backoff_all_caps_under_pressure(
        &mut self,
        factor: f64,
        uid_hotkeys: &HashMap<u16, String>,
    ) -> usize {
        if !cap_ramp_blocked_by_memory_pressure() {
            return 0;
        }
        let factor = factor.clamp(0.0, 1.0);
        let mut changed = 0usize;
        for (uid, cap) in self.adaptive_caps.iter_mut() {
            if *cap <= 1 {
                continue;
            }
            let decrement = ((*cap as f64) * factor).round() as usize;
            let decrement = decrement.max(1);
            let new_cap = cap.saturating_sub(decrement).max(1);
            if new_cap < *cap {
                let hotkey = uid_hotkeys.get(uid).cloned().unwrap_or_default();
                self.cap_events.push(CapEvent {
                    uid: *uid,
                    hotkey,
                    direction: CapDirection::Backoff,
                    cap_from: *cap,
                    cap_to: new_cap,
                    success_rate: 0.0,
                    at: Instant::now(),
                });
                *cap = new_cap;
                if let Some(r) = self.at_cap_results.get_mut(uid) {
                    r.clear();
                }
                changed += 1;
            }
        }
        if self.cap_events.len() > MAX_BUFFERED_CAP_EVENTS {
            let drop = self.cap_events.len() - MAX_BUFFERED_CAP_EVENTS;
            self.cap_events.drain(0..drop);
        }
        changed
    }

    pub fn save(&self) {
        let path = match &self.persistence_path {
            Some(p) => p,
            None => return,
        };

        let now_instant = Instant::now();
        let now_secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs(),
            Err(_) => 0,
        };

        let mut windows_json = serde_json::Map::new();
        for (uid, window) in &self.windows {
            let entries: Vec<serde_json::Value> = window
                .iter()
                .map(|(ts, success, time)| {
                    let elapsed = now_instant.saturating_duration_since(*ts).as_secs();
                    let abs_secs = now_secs.saturating_sub(elapsed);
                    serde_json::json!([abs_secs, *success, *time])
                })
                .collect();
            windows_json.insert(uid.to_string(), serde_json::Value::Array(entries));
        }

        let mut capacities_json = serde_json::Map::new();
        for (uid, cap) in &self.adaptive_caps {
            let results: Vec<bool> = self
                .at_cap_results
                .get(uid)
                .map(|r| r.iter().copied().collect())
                .unwrap_or_default();
            capacities_json.insert(uid.to_string(), serde_json::json!([*cap, results]));
        }

        let data = serde_json::json!({
            "windows": windows_json,
            "capacities": capacities_json,
        });

        match serde_json::to_string(&data) {
            Ok(json_str) => {
                if let Err(e) = sn2_types::atomic_write_json(path, json_str.as_bytes()) {
                    warn!(error = %e, "saving performance tracker");
                }
            }
            Err(e) => {
                warn!(error = %e, "serializing performance tracker");
            }
        }
    }

    fn load(&mut self) {
        let path = match &self.persistence_path {
            Some(p) => p,
            None => return,
        };
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "performance tracker load: read failed, preserving in-memory state");
                return;
            }
        };
        let parsed: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "performance tracker load: parse failed, starting fresh");
                return;
            }
        };

        let now_instant = Instant::now();
        let now_secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs(),
            Err(_) => return,
        };
        let ttl_secs = window_ttl().as_secs();

        if let Some(map) = parsed.get("windows").and_then(|v| v.as_object()) {
            for (uid_str, entries) in map {
                let uid: u16 = match uid_str.parse() {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                let arr = match entries.as_array() {
                    Some(a) => a,
                    None => continue,
                };
                let mut deque: VecDeque<WindowEntry> = VecDeque::new();
                for entry in arr {
                    let triple = match entry.as_array() {
                        Some(t) if t.len() == 3 => t,
                        _ => continue,
                    };
                    let abs_secs = match triple[0].as_u64() {
                        Some(s) => s,
                        None => continue,
                    };
                    let success = match triple[1].as_bool() {
                        Some(b) => b,
                        None => continue,
                    };
                    let rt = match triple[2].as_f64() {
                        Some(f) => f,
                        None => continue,
                    };
                    if now_secs.saturating_sub(abs_secs) > ttl_secs {
                        continue;
                    }
                    let elapsed = now_secs.saturating_sub(abs_secs);
                    let ts = now_instant
                        .checked_sub(Duration::from_secs(elapsed))
                        .unwrap_or(now_instant);
                    deque.push_back((ts, success, rt));
                }
                if !deque.is_empty() {
                    self.windows.insert(uid, deque);
                }
            }
        }

        if let Some(map) = parsed.get("capacities").and_then(|v| v.as_object()) {
            for (uid_str, entry) in map {
                let uid: u16 = match uid_str.parse() {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                let arr = match entry.as_array() {
                    Some(a) if a.len() == 2 => a,
                    _ => continue,
                };
                let cap = match arr[0].as_u64() {
                    Some(c) => (c as usize).clamp(1, RESTORED_CAP_MAX),
                    None => continue,
                };
                let results: VecDeque<bool> = arr[1]
                    .as_array()
                    .map(|v| v.iter().filter_map(|b| b.as_bool()).collect())
                    .unwrap_or_default();
                self.adaptive_caps.insert(uid, cap);
                self.at_cap_last_touched.insert(uid, Instant::now());
                if !results.is_empty() {
                    self.at_cap_results.insert(uid, results);
                }
            }
        }
    }

    pub fn snapshot(&self) -> HashMap<u16, (f64, usize)> {
        self.windows
            .iter()
            .map(|(&uid, w)| (uid, (self.miner_rel_speed(uid).unwrap_or(0.0), w.len())))
            .collect()
    }

    pub fn throughput_snapshot(&self) -> HashMap<u16, (f64, usize, usize)> {
        self.windows
            .iter()
            .map(|(&uid, w)| {
                let work = self.miner_delivered_work(uid);
                let cap = self.adaptive_caps.get(&uid).copied().unwrap_or(1);
                (uid, (work, cap, w.len()))
            })
            .collect()
    }

    pub fn sample_counts(&self) -> HashMap<u16, usize> {
        self.windows
            .iter()
            .map(|(&uid, w)| (uid, w.len()))
            .collect()
    }

    fn miner_delivered_work(&self, uid: u16) -> f64 {
        let (credit, debit, _) = self.delivered_breakdown(uid);
        (credit - debit).max(0.0)
    }

    pub fn delivered_breakdown(&self, uid: u16) -> (f64, f64, u64) {
        let buckets = match self.delivered_work.get(&uid) {
            Some(b) => b,
            None => return (0.0, 0.0, 0),
        };
        let ttl = window_ttl() + Duration::from_secs(DELIVERED_WORK_BUCKET_SECS);
        let mut credit = 0.0;
        let mut debit = 0.0;
        let mut uncredited = 0u64;
        for (start, bucket) in buckets {
            if start.elapsed() <= ttl {
                credit += bucket.credit;
                debit += bucket.debit;
                uncredited += bucket.uncredited_failures;
            }
        }
        (credit, debit, uncredited)
    }

    pub fn reset_uid(&mut self, uid: u16, hotkey: &str) {
        self.windows.remove(&uid);
        self.adaptive_caps.remove(&uid);
        self.at_cap_results.remove(&uid);
        self.at_cap_last_touched.remove(&uid);
        self.rel_speed.remove(&uid);
        self.unit_own_best.remove(&uid);
        self.miner_slowdown.remove(&uid);
        self.delivered_work.remove(&uid);
        self.pending_evictions.retain(|(u, _)| *u != uid);
        self.cap_events.retain(|e| e.hotkey != hotkey);
    }

    fn unit_reference(&self, work_key: &str) -> f64 {
        let samples = match self.unit_times.get(work_key) {
            Some(s) if s.len() >= UNIT_REFERENCE_MIN_SAMPLES => s,
            _ => return 0.0,
        };
        let mut times: Vec<f64> = samples.iter().map(|(_, t)| *t).collect();
        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((times.len() as f64 * CAPACITY_UNIT_REFERENCE_PERCENTILE) as usize)
            .min(times.len().saturating_sub(1));
        times[idx]
    }

    fn cached_unit_reference(&mut self, work_key: &str) -> f64 {
        if let Some((at, reference)) = self.unit_reference_cache.get(work_key) {
            if self.total_records.saturating_sub(*at) < UNIT_REFERENCE_REFRESH_RECORDS {
                return *reference;
            }
        }
        let reference = self.unit_reference(work_key);
        if reference > 0.0 {
            self.unit_reference_cache
                .insert(work_key.to_string(), (self.total_records, reference));
        }
        reference
    }

    fn miner_rel_speed(&self, uid: u16) -> Option<f64> {
        let samples = self.rel_speed.get(&uid)?;
        let ttl = window_ttl();
        let mut count = 0usize;
        let sum: f64 = samples
            .iter()
            .filter(|(ts, _)| ts.elapsed() <= ttl)
            .map(|(_, r)| {
                count += 1;
                *r
            })
            .sum();
        if count == 0 {
            return None;
        }
        Some(sum / count as f64)
    }

    /// How much a miner's recent latency has degraded relative to its own
    /// best on the same work units. 1.0 means it is serving at its unloaded
    /// speed (spare capacity); a higher value means the load we are giving it
    /// is congesting it. This is purely self-referential, so it cannot couple
    /// one miner's capacity to another's and cannot drive a feedback spiral.
    fn miner_saturation(&self, uid: u16) -> f64 {
        let samples = match self.miner_slowdown.get(&uid) {
            Some(s) => s,
            None => return 1.0,
        };
        let ttl = window_ttl();
        let mut count = 0usize;
        let sum: f64 = samples
            .iter()
            .filter(|(ts, _)| ts.elapsed() <= ttl)
            .map(|(_, s)| {
                count += 1;
                *s
            })
            .sum();
        if count == 0 {
            return 1.0;
        }
        sum / count as f64
    }

    fn update_adaptive_cap(&mut self, uid: u16, hotkey: &str) {
        let success_rate = match self.at_cap_results.get(&uid) {
            Some(r) if r.len() >= CAPACITY_MIN_AT_CAP => {
                r.iter().filter(|&&s| s).count() as f64 / r.len() as f64
            }
            _ => return,
        };

        let saturation = self.miner_saturation(uid);

        let current = self.adaptive_caps.entry(uid).or_insert(1);
        let cap_from = *current;
        let mut direction: Option<CapDirection> = None;

        if success_rate >= CAPACITY_RAMP_THRESHOLD {
            if cap_ramp_blocked_by_memory_pressure() {
                debug!(
                    uid,
                    cap = *current,
                    success_rate,
                    "cap ramp held back by validator memory pressure"
                );
                if let Some(r) = self.at_cap_results.get_mut(&uid) {
                    r.clear();
                }
                return;
            }
            if saturation <= CAPACITY_SATURATION_RAMP_CEILING {
                *current += 1;
                direction = Some(CapDirection::Ramp);
            } else if saturation > CAPACITY_SATURATION_TOLERANCE && *current > 1 {
                *current -= 1;
                direction = Some(CapDirection::Backoff);
            }
            if let Some(r) = self.at_cap_results.get_mut(&uid) {
                r.clear();
            }
        } else if success_rate < CAPACITY_BACKOFF_THRESHOLD && *current > 0 {
            *current -= 1;
            direction = if *current == 0 {
                self.pending_evictions.push((uid, hotkey.to_string()));
                Some(CapDirection::Evict)
            } else {
                Some(CapDirection::Backoff)
            };
            if let Some(r) = self.at_cap_results.get_mut(&uid) {
                r.clear();
            }
        }

        if let Some(direction) = direction {
            let cap_to = *current;
            self.cap_events.push(CapEvent {
                uid,
                hotkey: hotkey.to_string(),
                direction,
                cap_from,
                cap_to,
                success_rate,
                at: Instant::now(),
            });
            if self.cap_events.len() > MAX_BUFFERED_CAP_EVENTS {
                let drop = self.cap_events.len() - MAX_BUFFERED_CAP_EVENTS;
                self.cap_events.drain(0..drop);
            }
        }
    }

    pub fn cap_snapshot(&self) -> HashMap<u16, usize> {
        self.adaptive_caps.clone()
    }

    pub fn drain_cap_events(&mut self) -> Vec<CapEvent> {
        std::mem::take(&mut self.cap_events)
    }

    pub fn drain_pending_evictions(&mut self) -> Vec<(u16, String)> {
        std::mem::take(&mut self.pending_evictions)
    }

    pub fn decay_idle_caps(&mut self, uid_hotkeys: &HashMap<u16, String>) -> usize {
        let now = Instant::now();
        let idle = Duration::from_secs(CAP_DECAY_IDLE_SECS);
        let mut decayed = 0usize;
        let uids: Vec<u16> = self
            .adaptive_caps
            .iter()
            .filter(|(_, &cap)| cap > 0)
            .filter(|(uid, _)| {
                self.at_cap_last_touched
                    .get(uid)
                    .map(|t| now.duration_since(*t) > idle)
                    .unwrap_or(true)
            })
            .map(|(uid, _)| *uid)
            .collect();
        for uid in uids {
            let current = self.adaptive_caps.entry(uid).or_insert(1);
            if *current <= 1 {
                continue;
            }
            let cap_from = *current;
            *current -= 1;
            let cap_to = *current;
            decayed += 1;
            if let Some(r) = self.at_cap_results.get_mut(&uid) {
                r.clear();
            }
            let hotkey = uid_hotkeys.get(&uid).cloned().unwrap_or_default();
            let direction = CapDirection::Backoff;
            self.cap_events.push(CapEvent {
                uid,
                hotkey,
                direction,
                cap_from,
                cap_to,
                success_rate: 0.0,
                at: now,
            });
            if self.cap_events.len() > MAX_BUFFERED_CAP_EVENTS {
                let drop = self.cap_events.len() - MAX_BUFFERED_CAP_EVENTS;
                self.cap_events.drain(0..drop);
            }
        }
        decayed
    }

    pub fn rehabilitate(&mut self, uid: u16, hotkey: &str) {
        self.at_cap_last_touched.insert(uid, Instant::now());
        let current = match self.adaptive_caps.get_mut(&uid) {
            Some(c) if *c == 0 => c,
            _ => return,
        };
        let cap_from = *current;
        *current = 1;
        let cap_to = *current;
        if let Some(r) = self.at_cap_results.get_mut(&uid) {
            r.clear();
        }
        self.at_cap_last_touched.insert(uid, Instant::now());
        self.cap_events.push(CapEvent {
            uid,
            hotkey: hotkey.to_string(),
            direction: CapDirection::Rehab,
            cap_from,
            cap_to,
            success_rate: 0.0,
            at: Instant::now(),
        });
        if self.cap_events.len() > MAX_BUFFERED_CAP_EVENTS {
            let drop = self.cap_events.len() - MAX_BUFFERED_CAP_EVENTS;
            self.cap_events.drain(0..drop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tracker() -> PerformanceTracker {
        PerformanceTracker {
            windows: HashMap::new(),
            adaptive_caps: HashMap::new(),
            at_cap_results: HashMap::new(),
            cap_events: Vec::new(),
            pending_evictions: Vec::new(),
            at_cap_last_touched: HashMap::new(),
            unit_times: HashMap::new(),
            unit_reference_cache: HashMap::new(),
            rel_speed: HashMap::new(),
            unit_own_best: HashMap::new(),
            miner_slowdown: HashMap::new(),
            delivered_work: HashMap::new(),
            total_records: 0,
            persistence_path: None,
        }
    }

    #[test]
    fn adaptive_timeout_returns_default_with_few_samples() {
        let tracker = test_tracker();
        assert_eq!(tracker.adaptive_timeout(), CIRCUIT_TIMEOUT_SECONDS as f64);
    }

    #[test]
    fn parse_meminfo_extracts_avail_ratio() {
        let sample = "MemTotal:       32096780 kB\n\
                      MemFree:         1234567 kB\n\
                      MemAvailable:    8024195 kB\n\
                      Buffers:           12345 kB\n";
        let ratio = parse_meminfo_avail_ratio(sample).expect("ratio");
        assert!(
            (ratio - 0.25).abs() < 0.005,
            "ratio {ratio} should be ~0.25"
        );
    }

    #[test]
    fn parse_meminfo_returns_none_on_missing_fields() {
        assert!(parse_meminfo_avail_ratio("MemTotal: 1 kB\n").is_none());
        assert!(parse_meminfo_avail_ratio("MemAvailable: 1 kB\n").is_none());
        assert!(parse_meminfo_avail_ratio("").is_none());
    }

    #[test]
    fn record_populates_window() {
        let mut tracker = test_tracker();
        tracker.record(1, "hk", true, 5.0, false);
        tracker.record(1, "hk", true, 6.0, false);
        assert_eq!(tracker.windows.get(&1).unwrap().len(), 2);
    }

    #[test]
    fn record_reschedule_adds_penalty() {
        let mut tracker = test_tracker();
        tracker.record_reschedule(1);
        let window = tracker.windows.get(&1).unwrap();
        assert_eq!(window.len(), 1);
        let (_, success, time) = window[0];
        assert!(!success);
        assert_eq!(time, PERFORMANCE_RESCHEDULE_PENALTY);
    }

    #[test]
    fn reset_uid_clears_all_state() {
        let mut tracker = test_tracker();
        tracker.record(1, "hk", true, 5.0, true);
        tracker.adaptive_caps.insert(1, 4);
        tracker.reset_uid(1, "hk");
        assert!(!tracker.windows.contains_key(&1));
        assert!(!tracker.adaptive_caps.contains_key(&1));
        assert!(!tracker.at_cap_results.contains_key(&1));
    }

    #[test]
    fn adaptive_timeout_calculates_percentile_and_clamps() {
        let mut tracker = test_tracker();
        for i in 0..ADAPTIVE_TIMEOUT_MIN_SAMPLES {
            tracker.record(1, "hk", true, (i + 1) as f64, false);
        }
        let timeout = tracker.adaptive_timeout();
        let mut sorted: Vec<f64> = (1..=ADAPTIVE_TIMEOUT_MIN_SAMPLES)
            .map(|i| i as f64)
            .collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let idx =
            ((sorted.len() as f64 * ADAPTIVE_TIMEOUT_PERCENTILE) as usize).min(sorted.len() - 1);
        let expected =
            (sorted[idx] * ADAPTIVE_TIMEOUT_MULTIPLIER).min(CIRCUIT_TIMEOUT_SECONDS as f64);
        assert!(
            (timeout - expected).abs() < 1e-9,
            "timeout={timeout}, expected={expected}"
        );

        let mut tracker2 = test_tracker();
        for _ in 0..ADAPTIVE_TIMEOUT_MIN_SAMPLES {
            tracker2.record(1, "hk", true, 500.0, false);
        }
        assert_eq!(
            tracker2.adaptive_timeout(),
            CIRCUIT_TIMEOUT_SECONDS as f64,
            "should clamp to CIRCUIT_TIMEOUT_SECONDS"
        );
    }

    #[test]
    fn miner_capacities_default_to_one() {
        let mut tracker = test_tracker();
        tracker.record(1, "hk", true, 5.0, false);
        let caps = tracker.miner_capacities();
        assert_eq!(caps.get(&1).copied().unwrap_or(0), 1);
    }

    #[test]
    fn record_with_time_evicts_stale_entries() {
        let mut tracker = test_tracker();
        let now = Instant::now();
        let stale = now
            .checked_sub(window_ttl() + Duration::from_secs(60))
            .expect("Instant arithmetic");
        tracker.record_with_time(1, "hk", true, 5.0, false, "", stale);
        tracker.record_with_time(1, "hk", true, 6.0, false, "", now);
        let window = tracker.windows.get(&1).unwrap();
        assert_eq!(window.len(), 1);
        assert_eq!(window[0].2, 6.0);
    }

    #[test]
    fn cap_ramp_emits_event_with_from_to_and_rate() {
        let mut tracker = test_tracker();
        for _ in 0..CAPACITY_MIN_AT_CAP {
            tracker.record(1, "hk", true, 1.0, true);
        }
        let events = tracker.drain_cap_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].direction, CapDirection::Ramp);
        assert_eq!(events[0].cap_from, 1);
        assert_eq!(events[0].cap_to, 2);
        assert!((events[0].success_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cap_backoff_emits_event_when_already_above_one() {
        let mut tracker = test_tracker();
        for _ in 0..CAPACITY_MIN_AT_CAP {
            tracker.record(1, "hk", true, 1.0, true);
        }
        tracker.cap_events.clear();
        for _ in 0..CAPACITY_MIN_AT_CAP {
            tracker.record(1, "hk", false, 1.0, true);
        }
        let events = tracker.drain_cap_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].direction, CapDirection::Backoff);
        assert_eq!(events[0].cap_from, 2);
        assert_eq!(events[0].cap_to, 1);
    }

    #[test]
    fn cap_reflects_sustainable_concurrency_not_absolute_speed() {
        // Two miners, one 25x slower per proof, both with perfectly stable
        // latency. Capacity tracks each miner's own saturation, not its speed,
        // so a slow-but-stable miner is utilized just as fully as a fast one.
        let mut tracker = test_tracker();
        for _ in 0..400 {
            tracker.record(1, "fast", true, 2.0, true);
            tracker.record(2, "slow", true, 50.0, true);
        }
        let caps = tracker.cap_snapshot();
        let fast = caps.get(&1).copied().unwrap_or(0);
        let slow = caps.get(&2).copied().unwrap_or(0);
        assert!(
            fast > 4 && slow > 4,
            "both should ramp: fast={fast}, slow={slow}"
        );
        assert_eq!(
            fast, slow,
            "absolute speed must not affect capacity: fast={fast}, slow={slow}"
        );
    }

    #[test]
    fn sub_floor_latency_jitter_does_not_read_as_saturation() {
        let mut tracker = test_tracker();
        for i in 0..400 {
            let rt = if i % 2 == 0 { 0.05 } else { 0.2 };
            tracker.record(1, "fast", true, rt, true);
        }
        let cap = tracker.cap_snapshot().get(&1).copied().unwrap_or(0);
        assert!(
            cap > 4,
            "jitter below the latency floor must not block ramping: cap={cap}"
        );
    }

    #[test]
    fn saturation_between_thresholds_holds_cap_steady() {
        let mut tracker = test_tracker();
        for _ in 0..300 {
            tracker.record(1, "hk", true, 2.0, true);
        }
        assert!(tracker.cap_snapshot().get(&1).copied().unwrap_or(0) >= 4);
        for _ in 0..128 {
            tracker.record(1, "hk", true, 2.7, true);
        }
        let settled = tracker.cap_snapshot().get(&1).copied().unwrap_or(0);
        tracker.cap_events.clear();
        for _ in 0..(CAPACITY_MIN_AT_CAP * 4) {
            tracker.record(1, "hk", true, 2.7, true);
        }
        assert_eq!(
            tracker.cap_snapshot().get(&1).copied(),
            Some(settled),
            "saturation inside the deadband must neither ramp nor back off"
        );
        assert!(tracker.drain_cap_events().is_empty());
    }

    #[test]
    fn cap_backs_off_when_own_latency_degrades() {
        // Ramp on stable latency, then degrade it relative to the miner's own
        // best. The self-referential saturation signal must trim the cap.
        let mut tracker = test_tracker();
        for _ in 0..300 {
            tracker.record(1, "hk", true, 2.0, true);
        }
        let high = tracker.cap_snapshot().get(&1).copied().unwrap_or(0);
        assert!(high >= 4, "expected ramp-up before degradation, got {high}");
        tracker.cap_events.clear();
        for _ in 0..CAPACITY_MIN_AT_CAP {
            tracker.record(1, "hk", true, 100.0, true);
        }
        let events = tracker.drain_cap_events();
        assert!(
            events.iter().any(|e| e.direction == CapDirection::Backoff),
            "degrading own latency should trigger a backoff, got {events:?}"
        );
    }

    #[test]
    fn unit_reference_prices_delivered_work() {
        let mut tracker = test_tracker();
        for _ in 0..400 {
            tracker.record_keyed(0, "a", true, 100.0, false, "A");
            tracker.record_keyed(0, "a", true, 100.0, false, "A");
            tracker.record_keyed(3, "b", true, 1.0, false, "B");
            tracker.record_keyed(3, "b", true, 1.0, false, "B");
            tracker.record_keyed(1, "x", true, 50.0, true, "A");
            tracker.record_keyed(2, "y", true, 2.0, true, "B");
        }
        let snap = tracker.throughput_snapshot();
        let x = snap.get(&1).map(|&(w, _, _)| w).unwrap_or(0.0);
        let y = snap.get(&2).map(|&(w, _, _)| w).unwrap_or(0.0);
        assert!(
            x > y,
            "equal volume on a heavier unit must earn more: x={x}, y={y}"
        );
    }

    #[test]
    fn delivered_work_accrues_only_on_verified_success() {
        let mut tracker = test_tracker();
        for _ in 0..200 {
            tracker.record_keyed(1, "a", true, 2.0, false, "A");
            tracker.record_keyed(2, "b", true, 2.0, false, "A");
            tracker.record_keyed(2, "b", false, 2.0, false, "A");
        }
        for _ in 0..200 {
            tracker.record_keyed(1, "a", true, 2.0, false, "A");
        }
        let snap = tracker.throughput_snapshot();
        let w1 = snap.get(&1).map(|&(w, _, _)| w).unwrap_or(0.0);
        let w2 = snap.get(&2).map(|&(w, _, _)| w).unwrap_or(0.0);
        assert!(w1 > 0.0 && w2 > 0.0);
        assert!(
            w1 > w2 * 1.5,
            "failures must not earn work credit: w1={w1}, w2={w2}"
        );
    }

    #[test]
    fn failures_debit_referenced_work_only() {
        let mut tracker = test_tracker();
        for _ in 0..100 {
            tracker.record_keyed(1, "a", true, 2.0, false, "A");
            tracker.record_keyed(2, "b", true, 2.0, false, "A");
        }
        let before = tracker
            .throughput_snapshot()
            .get(&2)
            .map(|&(w, _, _)| w)
            .unwrap_or(0.0);
        assert!(before > 0.0);

        for _ in 0..10 {
            tracker.record_reschedule_keyed(2, "B");
        }
        let after_unreferenced = tracker
            .throughput_snapshot()
            .get(&2)
            .map(|&(w, _, _)| w)
            .unwrap_or(0.0);
        assert_eq!(before, after_unreferenced);

        for _ in 0..10 {
            tracker.record_reschedule_keyed(2, "A");
        }
        let after_referenced = tracker
            .throughput_snapshot()
            .get(&2)
            .map(|&(w, _, _)| w)
            .unwrap_or(0.0);
        assert!(
            after_referenced < after_unreferenced,
            "referenced failures must debit: before={after_unreferenced}, after={after_referenced}"
        );

        for _ in 0..200 {
            tracker.record_reschedule_keyed(2, "A");
        }
        let floored = tracker
            .throughput_snapshot()
            .get(&2)
            .map(|&(w, _, _)| w)
            .unwrap_or(f64::MIN);
        assert_eq!(floored, 0.0);
    }

    #[test]
    fn unmeasured_miner_ranks_zero_not_neutral() {
        let mut tracker = test_tracker();
        for _ in 0..(PERFORMANCE_MIN_SAMPLES + 10) {
            tracker.record_reschedule(3);
            tracker.record_keyed(4, "hk", true, 2.0, false, "A");
        }
        let rate = tracker
            .snapshot()
            .get(&3)
            .map(|&(r, _)| r)
            .expect("uid 3 tracked");
        assert_eq!(rate, 0.0);
        let (work, _, count) = tracker
            .throughput_snapshot()
            .get(&3)
            .copied()
            .expect("uid 3 tracked");
        assert_eq!(work, 0.0);
        assert!(count >= PERFORMANCE_MIN_SAMPLES);
    }

    #[test]
    fn drain_cap_events_clears_buffer() {
        let mut tracker = test_tracker();
        for _ in 0..CAPACITY_MIN_AT_CAP {
            tracker.record(1, "hk", true, 1.0, true);
        }
        assert_eq!(tracker.drain_cap_events().len(), 1);
        assert!(tracker.drain_cap_events().is_empty());
    }

    #[test]
    fn cap_event_buffer_is_bounded() {
        let mut tracker = test_tracker();
        for _ in 0..(MAX_BUFFERED_CAP_EVENTS + 50) {
            for _ in 0..CAPACITY_MIN_AT_CAP {
                tracker.record(1, "hk", true, 1.0, true);
            }
        }
        assert!(tracker.cap_events.len() <= MAX_BUFFERED_CAP_EVENTS);
    }

    #[test]
    fn reset_uid_purges_buffered_cap_events_for_that_hotkey() {
        let mut tracker = test_tracker();
        for _ in 0..CAPACITY_MIN_AT_CAP {
            tracker.record(1, "hk_a", true, 1.0, true);
        }
        for _ in 0..CAPACITY_MIN_AT_CAP {
            tracker.record(2, "hk_b", true, 1.0, true);
        }
        assert_eq!(tracker.cap_events.len(), 2);
        tracker.reset_uid(1, "hk_a");
        let remaining = tracker.drain_cap_events();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].hotkey, "hk_b");
    }

    #[test]
    fn reset_uid_purge_is_keyed_by_hotkey_not_uid_slot() {
        let mut tracker = test_tracker();
        for _ in 0..CAPACITY_MIN_AT_CAP {
            tracker.record(1, "hk_old", true, 1.0, true);
        }
        for _ in 0..CAPACITY_MIN_AT_CAP {
            tracker.record(1, "hk_new", true, 1.0, true);
        }
        assert_eq!(tracker.cap_events.len(), 2);
        tracker.reset_uid(1, "hk_old");
        let remaining = tracker.drain_cap_events();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].hotkey, "hk_new");
    }

    fn run_at_cap_failures(tracker: &mut PerformanceTracker, uid: u16, hotkey: &str, n: usize) {
        for _ in 0..n {
            tracker.record(uid, hotkey, false, 1.0, true);
        }
    }

    #[test]
    fn cap_ratchets_below_one_to_zero_on_sustained_failure() {
        let mut tracker = test_tracker();
        // Ramp to a higher cap first by running successful at-cap windows.
        for _ in 0..(CAPACITY_MIN_AT_CAP * 3) {
            tracker.record(1, "hk", true, 1.0, true);
        }
        assert!(tracker.adaptive_caps.get(&1).copied().unwrap_or(0) >= 2);
        // Now ratchet down with failure windows until we hit 0.
        for _ in 0..30 {
            run_at_cap_failures(&mut tracker, 1, "hk", CAPACITY_MIN_AT_CAP);
            if tracker.adaptive_caps.get(&1).copied() == Some(0) {
                break;
            }
        }
        assert_eq!(tracker.adaptive_caps.get(&1).copied(), Some(0));
    }

    #[test]
    fn cap_drop_to_zero_emits_evict_event_and_pending_eviction() {
        let mut tracker = test_tracker();
        // Force the at-cap window full of failures; cap starts at default 1.
        run_at_cap_failures(&mut tracker, 7, "hk_dead", CAPACITY_MIN_AT_CAP);
        assert_eq!(tracker.adaptive_caps.get(&7).copied(), Some(0));
        let events = tracker.drain_cap_events();
        let evict = events
            .iter()
            .find(|e| matches!(e.direction, CapDirection::Evict))
            .expect("expected Evict event");
        assert_eq!(evict.uid, 7);
        assert_eq!(evict.hotkey, "hk_dead");
        assert_eq!(evict.cap_from, 1);
        assert_eq!(evict.cap_to, 0);
        let evicted = tracker.drain_pending_evictions();
        assert_eq!(evicted, vec![(7, "hk_dead".to_string())]);
    }

    #[test]
    fn drain_pending_evictions_clears() {
        let mut tracker = test_tracker();
        run_at_cap_failures(&mut tracker, 7, "hk_dead", CAPACITY_MIN_AT_CAP);
        assert!(!tracker.drain_pending_evictions().is_empty());
        assert!(tracker.drain_pending_evictions().is_empty());
    }

    #[test]
    fn restored_caps_floor_at_one() {
        let dir = std::env::temp_dir().join(format!("sn2_perf_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("performance_tracker.json");
        let mut tracker = PerformanceTracker::new_with_persistence(path.clone());
        tracker.record_keyed(9, "hk", true, 2.0, false, "A");
        tracker.adaptive_caps.insert(9, 0);
        tracker.save();
        let restored = PerformanceTracker::new_with_persistence(path.clone());
        assert_eq!(
            restored.adaptive_caps.get(&9).copied(),
            Some(1),
            "a persisted zero cap must revive as one, never a dispatch black hole"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn idle_decay_floors_at_one_without_eviction() {
        let mut tracker = test_tracker();
        tracker.adaptive_caps.insert(3, 2);
        let hotkeys: HashMap<u16, String> = [(3u16, "hk".to_string())].into_iter().collect();
        assert_eq!(tracker.decay_idle_caps(&hotkeys), 1);
        assert_eq!(tracker.adaptive_caps.get(&3).copied(), Some(1));
        assert_eq!(tracker.decay_idle_caps(&hotkeys), 0);
        assert_eq!(tracker.adaptive_caps.get(&3).copied(), Some(1));
        assert!(tracker.drain_pending_evictions().is_empty());
        assert!(!tracker
            .drain_cap_events()
            .iter()
            .any(|e| matches!(e.direction, CapDirection::Evict)));
    }

    #[test]
    fn rehabilitate_restores_cap_from_zero_to_one() {
        let mut tracker = test_tracker();
        run_at_cap_failures(&mut tracker, 7, "hk_dead", CAPACITY_MIN_AT_CAP);
        let _ = tracker.drain_cap_events();
        tracker.rehabilitate(7, "hk_dead");
        assert_eq!(tracker.adaptive_caps.get(&7).copied(), Some(1));
        let events = tracker.drain_cap_events();
        assert!(events
            .iter()
            .any(|e| matches!(e.direction, CapDirection::Rehab)));
        // at-cap window cleared so the next probe doesn't carry old failures.
        assert!(tracker
            .at_cap_results
            .get(&7)
            .map(|r| r.is_empty())
            .unwrap_or(true));
    }

    #[test]
    fn rehabilitate_is_noop_when_cap_is_nonzero() {
        let mut tracker = test_tracker();
        tracker.adaptive_caps.insert(7, 3);
        tracker.rehabilitate(7, "hk_alive");
        assert_eq!(tracker.adaptive_caps.get(&7).copied(), Some(3));
        let events = tracker.drain_cap_events();
        assert!(events.is_empty());
    }
}
