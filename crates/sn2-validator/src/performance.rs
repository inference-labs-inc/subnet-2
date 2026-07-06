use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sn2_types::{
    ADAPTIVE_TIMEOUT_MIN_SAMPLES, ADAPTIVE_TIMEOUT_MULTIPLIER, ADAPTIVE_TIMEOUT_PERCENTILE,
    BLOCK_TIME_SECS, CAPACITY_ADJUST_INTERVAL_SECS, CAPACITY_LATENCY_BUDGET_SECS,
    CAPACITY_RAMP_MIN_AVAIL_MEM_RATIO, CAPACITY_RATE_BIN_SECS, CAPACITY_RATE_FILTER_BINS,
    CAPACITY_RATE_WINDOW_BINS, CAPACITY_STEP_FRACTION, CAPACITY_TARGET_DEADBAND,
    CAPACITY_TARGET_HEADROOM, CAPACITY_UNIT_REFERENCE_PERCENTILE, CIRCUIT_TIMEOUT_SECONDS,
    DELIVERED_WORK_BUCKET_SECS, DELIVERED_WORK_HALF_LIFE_SECS, FAILURE_DEBIT_MULTIPLIER,
    PERFORMANCE_MIN_SAMPLES, PERFORMANCE_RESCHEDULE_PENALTY, PERFORMANCE_WINDOW_SIZE,
    VERIFICATION_WINDOW_BLOCKS,
};
use tracing::warn;

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
    #[allow(dead_code)]
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
    // Per-completion samples of the miner's own-best latency for the unit
    // just completed. The median over this window is the miner's uncongested
    // service time weighted by the work mix it is actually being sent.
    min_service: HashMap<u16, VecDeque<(Instant, f64)>>,
    delivered_work: HashMap<u16, VecDeque<(Instant, WorkBucket)>>,
    completion_bins: HashMap<u16, VecDeque<(Instant, u32)>>,
    cap_last_adjusted: HashMap<u16, Instant>,
    total_records: u64,
    persistence_path: Option<PathBuf>,
}

const CAP_DECAY_IDLE_SECS: u64 = 600;

const UNIT_REFERENCE_REFRESH_RECORDS: u64 = 64;
const UNIT_TIMES_CAP: usize = 256;
const UNIT_REFERENCE_MIN_SAMPLES: usize = 8;
const REL_SPEED_WINDOW: usize = 64;

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
            min_service: HashMap::new(),
            delivered_work: HashMap::new(),
            completion_bins: HashMap::new(),
            cap_last_adjusted: HashMap::new(),
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
                let min_samples = self.min_service.entry(uid).or_default();
                min_samples.push_back((now, own_best));
                evict_timed(min_samples, REL_SPEED_WINDOW);
            }

            if own_best > 0.0 {
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

        if success {
            let bins = self.completion_bins.entry(uid).or_default();
            let bin_len = Duration::from_secs(CAPACITY_RATE_BIN_SECS);
            match bins.back_mut() {
                Some((start, count)) if now.duration_since(*start) < bin_len => *count += 1,
                _ => bins.push_back((now, 1)),
            }
            while bins.len() > CAPACITY_RATE_FILTER_BINS {
                bins.pop_front();
            }
            self.at_cap_last_touched.insert(uid, now);
            self.update_adaptive_cap(uid, hotkey, now);
        }
        let _ = was_at_capacity;
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
        let buckets = match self.delivered_work.get(&uid) {
            Some(b) => b,
            None => return 0.0,
        };
        let ttl = window_ttl() + Duration::from_secs(DELIVERED_WORK_BUCKET_SECS);
        let half_life = DELIVERED_WORK_HALF_LIFE_SECS as f64;
        let mut work = 0.0;
        for (start, bucket) in buckets {
            let age = start.elapsed();
            if age <= ttl {
                let decay = 0.5_f64.powf(age.as_secs_f64() / half_life);
                work += (bucket.credit - bucket.debit) * decay;
            }
        }
        work.max(0.0)
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
        self.min_service.remove(&uid);
        self.delivered_work.remove(&uid);
        self.completion_bins.remove(&uid);
        self.cap_last_adjusted.remove(&uid);
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

    fn delivered_rate_estimate(&self, uid: u16, now: Instant) -> f64 {
        let bins = match self.completion_bins.get(&uid) {
            Some(b) if !b.is_empty() => b,
            _ => return 0.0,
        };
        let bin_len = CAPACITY_RATE_BIN_SECS as f64;
        let window_len = CAPACITY_RATE_WINDOW_BINS as f64 * bin_len;
        let horizon =
            Duration::from_secs(CAPACITY_RATE_BIN_SECS * CAPACITY_RATE_FILTER_BINS as u64);
        let samples: Vec<(Instant, u32)> = bins
            .iter()
            .filter(|(start, _)| now.duration_since(*start) <= horizon)
            .copied()
            .collect();
        let mut best = 0.0_f64;
        for i in 0..samples.len() {
            let window_start = samples[i].0;
            let sum: u32 = samples[i..]
                .iter()
                .take_while(|(s, _)| s.duration_since(window_start).as_secs_f64() < window_len)
                .map(|(_, c)| c)
                .sum();
            best = best.max(sum as f64 / window_len);
        }
        best
    }

    /// The miner's uncongested service time, weighted by the work mix it is
    /// actually completing: the median of per-completion own-best latencies.
    /// Window medians cannot serve here — observed response times include
    /// miner-side queueing, so by Little's law `rate x median` tracks
    /// whatever depth the validator itself keeps in flight and would confirm
    /// any cap. Own-best latencies are pinned per unit for a full window and
    /// do not inflate while the validator is congesting the miner.
    fn uncongested_service_time(&self, uid: u16) -> f64 {
        let samples = match self.min_service.get(&uid) {
            Some(s) if !s.is_empty() => s,
            _ => return 0.0,
        };
        let ttl = window_ttl();
        let mut times: Vec<f64> = samples
            .iter()
            .filter(|(ts, _)| ts.elapsed() <= ttl)
            .map(|(_, t)| *t)
            .collect();
        if times.is_empty() {
            return 0.0;
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        times[times.len() / 2]
    }

    fn update_adaptive_cap(&mut self, uid: u16, hotkey: &str, now: Instant) {
        let due = self
            .cap_last_adjusted
            .get(&uid)
            .map(|t| now.duration_since(*t).as_secs() >= CAPACITY_ADJUST_INTERVAL_SECS)
            .unwrap_or(true);
        if !due {
            return;
        }
        if cap_ramp_blocked_by_memory_pressure() {
            return;
        }
        self.cap_last_adjusted.insert(uid, now);
        let rate = self.delivered_rate_estimate(uid, now);
        let uncongested = self.uncongested_service_time(uid);

        // Little's law at the throughput knee: sustaining the delivered rate
        // requires rate x uncongested-service-time units in flight. Below the
        // knee the delivered rate rises with the cap, so the target is
        // self-raising and doubles as the probe out of a cap-limited
        // measurement; at the knee the rate plateaus while own-best latencies
        // hold, so the target pins. The latency-budget term keeps a queueing
        // floor for sub-budget service times, where a depth of rate x budget
        // costs no measurable latency.
        let knee_depth = rate * uncongested;
        let target = (knee_depth * CAPACITY_TARGET_HEADROOM)
            .max(rate * CAPACITY_LATENCY_BUDGET_SECS)
            .max(1.0);

        let current = self.adaptive_caps.entry(uid).or_insert(1);
        let cap_from = *current;
        let step = ((cap_from as f64 * CAPACITY_STEP_FRACTION).ceil() as usize).max(1);
        let target_cap = target.round() as usize;
        let cap_to = if target_cap > cap_from {
            (cap_from + step).min(target_cap)
        } else if (cap_from as f64) > target * (1.0 + CAPACITY_TARGET_DEADBAND) {
            cap_from.saturating_sub(step).max(target_cap).max(1)
        } else {
            cap_from
        };
        if cap_to == cap_from {
            return;
        }
        *current = cap_to;
        let direction = if cap_to > cap_from {
            CapDirection::Ramp
        } else {
            CapDirection::Backoff
        };
        self.cap_events.push(CapEvent {
            uid,
            hotkey: hotkey.to_string(),
            direction,
            cap_from,
            cap_to,
            success_rate: rate,
            at: now,
        });
        if self.cap_events.len() > MAX_BUFFERED_CAP_EVENTS {
            let drop = self.cap_events.len() - MAX_BUFFERED_CAP_EVENTS;
            self.cap_events.drain(0..drop);
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
            min_service: HashMap::new(),
            delivered_work: HashMap::new(),
            completion_bins: HashMap::new(),
            cap_last_adjusted: HashMap::new(),
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
        let base = Instant::now();
        drive_rate(&mut tracker, 1, 40, 120, base);
        let events = tracker.drain_cap_events();
        assert!(!events.is_empty());
        assert_eq!(events[0].direction, CapDirection::Ramp);
        assert!(events[0].cap_to > events[0].cap_from);
        assert!(events[0].success_rate >= 0.0);
    }

    #[test]
    fn cap_backoff_emits_event_when_rate_falls() {
        let mut tracker = test_tracker();
        let base = Instant::now();
        let mid = drive_rate(&mut tracker, 1, 40, 300, base);
        tracker.cap_events.clear();
        drive_rate(&mut tracker, 1, 4, 900, mid + Duration::from_secs(1));
        let events = tracker.drain_cap_events();
        assert!(events.iter().any(|e| e.direction == CapDirection::Backoff));
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
        assert!(
            (before - after_unreferenced).abs() < before * 1e-4,
            "unreferenced debit must not change work: {before} vs {after_unreferenced}"
        );

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
    fn delivered_work_decays_when_delivery_stops() {
        let mut tracker = test_tracker();
        let now = Instant::now();
        let old = now.checked_sub(Duration::from_secs(3 * 3600)).unwrap();
        for (uid, at) in [(1u16, old), (2u16, now)] {
            for _ in 0..50 {
                tracker.record_with_time(uid, "hk", true, 1.0, false, "A", at);
            }
        }
        let stale = tracker.miner_delivered_work(1);
        let fresh = tracker.miner_delivered_work(2);
        assert!(fresh > 0.0);
        assert!(
            stale < fresh * 0.5,
            "3h-old work must decay well below fresh work: stale={stale} fresh={fresh}"
        );
        assert!(
            stale > fresh * 0.1,
            "decay is a curve, not a cliff: stale={stale} fresh={fresh}"
        );
    }

    #[test]
    fn steady_delivery_props_up_score() {
        let mut tracker = test_tracker();
        let now = Instant::now();
        for h in 0..6u64 {
            let at = now.checked_sub(Duration::from_secs(h * 3600)).unwrap();
            for _ in 0..10 {
                tracker.record_with_time(3, "hk", true, 1.0, false, "A", at);
            }
        }
        let continuous = tracker.miner_delivered_work(3);
        for _ in 0..10 {
            tracker.record_with_time(4, "hk", true, 1.0, false, "A", now);
        }
        let single_hour = tracker.miner_delivered_work(4);
        assert!(
            continuous > single_hour * 2.0,
            "sustained delivery must outscore a single fresh burst: continuous={continuous} single={single_hour}"
        );
    }

    fn drive_rate(
        tracker: &mut PerformanceTracker,
        uid: u16,
        per_sec: usize,
        secs: u64,
        base: Instant,
    ) -> Instant {
        let mut now = base;
        for s in 0..secs {
            for i in 0..per_sec {
                let jitter = 0.005 + ((s as usize + i) % 40) as f64 * 0.001;
                now = base
                    + Duration::from_millis(s * 1000 + (i as u64 * 1000 / per_sec.max(1) as u64));
                tracker.record_with_time(uid, "hk", true, jitter, true, "A", now);
            }
        }
        now
    }

    fn drive_closed_loop(
        tracker: &mut PerformanceTracker,
        uid: u16,
        service_time: f64,
        demand_per_sec: usize,
        secs: u64,
        base: Instant,
    ) -> Instant {
        let mut now = base;
        for s in 0..secs {
            let cap = tracker
                .cap_snapshot()
                .get(&uid)
                .copied()
                .unwrap_or(1)
                .max(1);
            let deliverable = ((cap as f64 / service_time) as usize)
                .min(demand_per_sec)
                .max(1);
            for i in 0..deliverable {
                now =
                    base + Duration::from_millis(s * 1000 + (i as u64 * 1000 / deliverable as u64));
                tracker.record_with_time(uid, "hk", true, service_time, true, "A", now);
            }
        }
        now
    }

    /// Closed loop with miner-side queueing, the regime the constant-latency
    /// drivers cannot express: the validator keeps the cap full, the miner
    /// completes at most `concurrency / base_service` units per second, and
    /// observed latency inflates by the overcommit ratio per Little's law.
    /// Response times above the knee therefore rise with the cap itself,
    /// which is exactly the feedback that made a window-median utilization
    /// signal self-confirming.
    fn drive_queued_closed_loop(
        tracker: &mut PerformanceTracker,
        uid: u16,
        base_service: f64,
        concurrency: usize,
        secs: u64,
        base: Instant,
    ) -> Instant {
        let mut now = base;
        let mut carry = 0.0_f64;
        for s in 0..secs {
            let cap = tracker
                .cap_snapshot()
                .get(&uid)
                .copied()
                .unwrap_or(1)
                .max(1);
            let in_flight = cap as f64;
            let overcommit = (in_flight / concurrency as f64).max(1.0);
            let observed = base_service * overcommit;
            let deliver_f = in_flight.min(concurrency as f64) / base_service + carry;
            let deliverable = deliver_f as usize;
            carry = deliver_f - deliverable as f64;
            for i in 0..deliverable {
                now =
                    base + Duration::from_millis(s * 1000 + (i as u64 * 1000 / deliverable as u64));
                tracker.record_with_time(uid, "hk", true, observed, true, "A", now);
            }
        }
        now
    }

    #[test]
    fn saturated_miner_cap_pins_at_knee_without_runaway() {
        let mut tracker = test_tracker();
        tracker.adaptive_caps.insert(1, 1);
        let base = Instant::now();
        // Knee depth = concurrency = 10. The cap must settle at
        // headroom x knee and stay there, not ramp without bound on the
        // congestion-inflated utilization signal.
        drive_queued_closed_loop(&mut tracker, 1, 0.5, 10, 1800, base);
        let cap = tracker.cap_snapshot().get(&1).copied().unwrap_or(0);
        assert!(
            (12..=25).contains(&cap),
            "cap {cap} must pin near the knee depth of 10, neither collapsing nor running away"
        );
    }

    #[test]
    fn slow_service_cap_holds_knee_depth_not_budget_floor() {
        let mut tracker = test_tracker();
        tracker.adaptive_caps.insert(1, 1);
        let base = Instant::now();
        // Five-second units at concurrency 100: mu = 20/s, knee depth = 100.
        // A fixed latency-budget target of rate x budget = 15 sits an order
        // of magnitude below the sustaining depth; tracking it starves the
        // miner. The cap must hold the knee, bounded above by headroom.
        drive_queued_closed_loop(&mut tracker, 1, 5.0, 100, 3600, base);
        let cap = tracker.cap_snapshot().get(&1).copied().unwrap_or(0);
        assert!(
            (120..=200).contains(&cap),
            "cap {cap} must sustain the knee depth of 100, not track the budget floor of 15"
        );
    }

    #[test]
    fn saturated_steady_state_emits_no_capacity_events() {
        let mut tracker = test_tracker();
        tracker.adaptive_caps.insert(1, 1);
        let base = Instant::now();
        let mid = drive_queued_closed_loop(&mut tracker, 1, 0.5, 10, 1200, base);
        tracker.cap_events.clear();
        drive_queued_closed_loop(&mut tracker, 1, 0.5, 10, 600, mid + Duration::from_secs(1));
        let events = tracker.drain_cap_events();
        assert!(
            events.len() <= 2,
            "converged steady state must be silent, got {} events",
            events.len()
        );
    }

    #[test]
    fn closed_loop_escapes_floor_when_rate_is_cap_limited() {
        let mut tracker = test_tracker();
        tracker.adaptive_caps.insert(1, 1);
        let base = Instant::now();
        drive_closed_loop(&mut tracker, 1, 0.5, 1000, 900, base);
        let cap = tracker.cap_snapshot().get(&1).copied().unwrap_or(0);
        assert!(
            cap >= 20,
            "cap-limited miner must ramp out of the floor: cap={cap}"
        );
    }

    #[test]
    fn closed_loop_settles_at_demand_when_supply_limited() {
        let mut tracker = test_tracker();
        tracker.adaptive_caps.insert(1, 64);
        let base = Instant::now();
        drive_closed_loop(&mut tracker, 1, 0.05, 8, 900, base);
        let cap = tracker.cap_snapshot().get(&1).copied().unwrap_or(0);
        assert!(
            cap < 32,
            "supply-limited miner must not hold inflated cap: cap={cap}"
        );
        assert!(cap >= 1, "cap never drops below floor");
    }

    #[test]
    fn cap_converges_to_delivered_rate_times_budget() {
        let mut tracker = test_tracker();
        let base = Instant::now();
        drive_rate(&mut tracker, 1, 40, 600, base);
        let cap = tracker.cap_snapshot().get(&1).copied().unwrap_or(0);
        let target = (40.0 * CAPACITY_LATENCY_BUDGET_SECS) as usize;
        assert!(
            cap >= target * 8 / 10 && cap <= target * 15 / 10,
            "cap {cap} should settle near rate*budget {target}"
        );
    }

    #[test]
    fn cap_tracks_rate_decrease() {
        let mut tracker = test_tracker();
        let base = Instant::now();
        let mid = drive_rate(&mut tracker, 1, 40, 600, base);
        let high = tracker.cap_snapshot().get(&1).copied().unwrap_or(0);
        drive_rate(&mut tracker, 1, 8, 900, mid + Duration::from_secs(1));
        let low = tracker.cap_snapshot().get(&1).copied().unwrap_or(0);
        assert!(
            low < high / 2,
            "cap must follow rate down: high={high} low={low}"
        );
    }

    #[test]
    fn latency_noise_does_not_cause_oscillation() {
        let mut tracker = test_tracker();
        let base = Instant::now();
        drive_rate(&mut tracker, 1, 40, 300, base);
        tracker.cap_events.clear();
        drive_rate(&mut tracker, 1, 40, 600, base + Duration::from_secs(301));
        let events = tracker.drain_cap_events();
        let mut reversals = 0;
        for w in events.windows(2) {
            if w[0].direction != w[1].direction {
                reversals += 1;
            }
        }
        assert!(
            reversals <= events.len() / 3 + 2,
            "steady rate must not flap: {reversals} reversals in {} events",
            events.len()
        );
    }

    #[test]
    fn drain_cap_events_clears_buffer() {
        let mut tracker = test_tracker();
        let base = Instant::now();
        drive_rate(&mut tracker, 1, 40, 120, base);
        assert!(!tracker.drain_cap_events().is_empty());
        assert!(tracker.drain_cap_events().is_empty());
    }

    #[test]
    fn cap_event_buffer_is_bounded() {
        let mut tracker = test_tracker();
        let base = Instant::now();
        for i in 0..(MAX_BUFFERED_CAP_EVENTS + 50) {
            tracker.cap_events.push(CapEvent {
                uid: 1,
                hotkey: "hk".to_string(),
                direction: CapDirection::Ramp,
                cap_from: i,
                cap_to: i + 1,
                success_rate: 1.0,
                at: base,
            });
        }
        drive_rate(&mut tracker, 1, 40, 60, base);
        assert!(tracker.cap_events.len() <= MAX_BUFFERED_CAP_EVENTS + 50);
        drive_rate(&mut tracker, 1, 40, 60, base + Duration::from_secs(61));
        assert!(tracker.cap_events.len() <= MAX_BUFFERED_CAP_EVENTS);
    }

    #[test]
    fn reset_uid_purges_buffered_cap_events_for_that_hotkey() {
        let mut tracker = test_tracker();
        let base = Instant::now();
        drive_rate(&mut tracker, 1, 40, 120, base);
        drive_rate(&mut tracker, 2, 40, 120, base);
        tracker.cap_events.iter_mut().for_each(|e| {
            if e.uid == 2 {
                e.hotkey = "hk_b".to_string();
            }
        });
        tracker.reset_uid(1, "hk");
        let remaining = tracker.drain_cap_events();
        assert!(!remaining.is_empty());
        assert!(remaining.iter().all(|e| e.hotkey == "hk_b"));
    }

    #[test]
    fn reset_uid_purge_is_keyed_by_hotkey_not_uid_slot() {
        let mut tracker = test_tracker();
        let base = Instant::now();
        drive_rate(&mut tracker, 1, 40, 120, base);
        let mid = tracker.cap_events.len();
        assert!(mid > 0);
        drive_rate(&mut tracker, 1, 40, 120, base + Duration::from_secs(121));
        tracker
            .cap_events
            .iter_mut()
            .skip(mid)
            .for_each(|e| e.hotkey = "hk_new".to_string());
        tracker.reset_uid(1, "hk");
        let remaining = tracker.drain_cap_events();
        assert!(remaining.iter().all(|e| e.hotkey == "hk_new"));
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
        tracker.adaptive_caps.insert(7, 0);
        tracker.rehabilitate(7, "hk_dead");
        assert_eq!(tracker.adaptive_caps.get(&7).copied(), Some(1));
        let events = tracker.drain_cap_events();
        assert!(events
            .iter()
            .any(|e| matches!(e.direction, CapDirection::Rehab)));
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
