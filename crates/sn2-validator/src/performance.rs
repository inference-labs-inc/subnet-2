use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sn2_types::{
    ADAPTIVE_TIMEOUT_MIN_SAMPLES, ADAPTIVE_TIMEOUT_MULTIPLIER, ADAPTIVE_TIMEOUT_PERCENTILE,
    BLOCK_TIME_SECS, CAPACITY_BACKOFF_THRESHOLD, CAPACITY_MIN_AT_CAP, CAPACITY_RAMP_THRESHOLD,
    CAPACITY_WINDOW_SIZE, CIRCUIT_TIMEOUT_SECONDS, PERFORMANCE_MIN_SAMPLES,
    PERFORMANCE_RESCHEDULE_PENALTY, PERFORMANCE_SCORING_PERCENTILE, VERIFICATION_WINDOW_BLOCKS,
};
use tracing::warn;

type WindowEntry = (Instant, bool, f64);

const MAX_BUFFERED_CAP_EVENTS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapDirection {
    Ramp,
    Backoff,
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
    persistence_path: Option<PathBuf>,
}

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
            *current += 1;
            direction = Some(CapDirection::Ramp);
            if let Some(r) = self.at_cap_results.get_mut(&uid) {
                r.clear();
            }
        } else if success_rate < CAPACITY_BACKOFF_THRESHOLD && *current > 1 {
            *current -= 1;
            direction = Some(CapDirection::Backoff);
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
            persistence_path: None,
        }
    }

    #[test]
    fn adaptive_timeout_returns_default_with_few_samples() {
        let tracker = test_tracker();
        assert_eq!(tracker.adaptive_timeout(), CIRCUIT_TIMEOUT_SECONDS as f64);
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
}
