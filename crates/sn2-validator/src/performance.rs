use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sn2_types::{
    ADAPTIVE_TIMEOUT_MIN_SAMPLES, ADAPTIVE_TIMEOUT_MULTIPLIER, ADAPTIVE_TIMEOUT_PERCENTILE,
    BLOCK_TIME_SECS, CAPACITY_BACKOFF_THRESHOLD, CAPACITY_MIN_AT_CAP,
    CAPACITY_RAMP_MIN_AVAIL_MEM_RATIO, CAPACITY_RAMP_THRESHOLD, CAPACITY_WINDOW_SIZE,
    CIRCUIT_TIMEOUT_SECONDS, PERFORMANCE_MIN_SAMPLES, PERFORMANCE_RESCHEDULE_PENALTY,
    PERFORMANCE_SCORING_PERCENTILE, VERIFICATION_WINDOW_BLOCKS,
};
use tracing::{debug, warn};

#[cfg(target_os = "linux")]
fn host_memory_available_ratio() -> Option<f64> {
    let raw = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo_avail_ratio(&raw)
}

#[cfg(not(target_os = "linux"))]
fn host_memory_available_ratio() -> Option<f64> {
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

fn cap_ramp_blocked_by_memory_pressure() -> bool {
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
    persistence_path: Option<PathBuf>,
}

const CAP_DECAY_IDLE_SECS: u64 = 600;

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
            persistence_path: Some(path),
        };
        tracker.load();
        tracker
    }

    pub fn record(
        &mut self,
        uid: u16,
        hotkey: &str,
        success: bool,
        response_time: f64,
        was_at_capacity: bool,
    ) {
        self.record_with_time(
            uid,
            hotkey,
            success,
            response_time,
            was_at_capacity,
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
        now: Instant,
    ) {
        let window = self.windows.entry(uid).or_default();
        window.push_back((now, success, response_time));
        evict_expired(window);

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

    pub fn record_reschedule(&mut self, uid: u16) {
        let window = self.windows.entry(uid).or_default();
        window.push_back((Instant::now(), false, PERFORMANCE_RESCHEDULE_PENALTY));
        evict_expired(window);
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
                    Some(c) => c as usize,
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
        let reference = self.scoring_reference_time();
        self.windows
            .iter()
            .map(|(&uid, w)| (uid, (Self::uid_rate(w, reference), w.len())))
            .collect()
    }

    pub fn throughput_snapshot(&self) -> HashMap<u16, (f64, usize, usize)> {
        let reference = self.scoring_reference_time();
        self.windows
            .iter()
            .map(|(&uid, w)| {
                let rate = Self::uid_rate(w, reference);
                let cap = if w.len() < PERFORMANCE_MIN_SAMPLES {
                    1
                } else {
                    self.adaptive_caps.get(&uid).copied().unwrap_or(1)
                };
                (uid, (rate, cap, w.len()))
            })
            .collect()
    }

    fn scoring_reference_time(&self) -> f64 {
        let mut times: Vec<f64> = self
            .windows
            .values()
            .flat_map(|w| w.iter().filter(|(_, s, _)| *s).map(|(_, _, t)| *t))
            .collect();

        if times.len() < PERFORMANCE_MIN_SAMPLES {
            return CIRCUIT_TIMEOUT_SECONDS as f64;
        }

        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((times.len() as f64 * PERFORMANCE_SCORING_PERCENTILE) as usize)
            .min(times.len().saturating_sub(1));
        times[idx].max(1.0)
    }

    fn uid_rate(w: &VecDeque<WindowEntry>, reference: f64) -> f64 {
        if w.is_empty() {
            return 0.0;
        }
        let mut total = 0.0;
        for &(_, success, rt) in w {
            if rt == PERFORMANCE_RESCHEDULE_PENALTY {
                total += PERFORMANCE_RESCHEDULE_PENALTY;
            } else if !success {
            } else if rt <= 0.0 {
                total += 2.0;
            } else {
                total += (reference / rt).min(2.0);
            }
        }
        (total / w.len() as f64).max(0.0)
    }

    pub fn reset_uid(&mut self, uid: u16, hotkey: &str) {
        self.windows.remove(&uid);
        self.adaptive_caps.remove(&uid);
        self.at_cap_results.remove(&uid);
        self.at_cap_last_touched.remove(&uid);
        self.pending_evictions.retain(|(u, _)| *u != uid);
        self.cap_events.retain(|e| e.hotkey != hotkey);
    }

    fn update_adaptive_cap(&mut self, uid: u16, hotkey: &str) {
        let success_rate = match self.at_cap_results.get(&uid) {
            Some(r) if r.len() >= CAPACITY_MIN_AT_CAP => {
                r.iter().filter(|&&s| s).count() as f64 / r.len() as f64
            }
            _ => return,
        };

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
            *current += 1;
            direction = Some(CapDirection::Ramp);
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
            if *current == 0 {
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
            let direction = if cap_to == 0 {
                self.pending_evictions.push((uid, hotkey.clone()));
                CapDirection::Evict
            } else {
                CapDirection::Backoff
            };
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
        tracker.record_with_time(1, "hk", true, 5.0, false, stale);
        tracker.record_with_time(1, "hk", true, 6.0, false, now);
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
