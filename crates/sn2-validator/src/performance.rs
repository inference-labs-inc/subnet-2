use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use sn2_types::{
    ADAPTIVE_TIMEOUT_MIN_SAMPLES, ADAPTIVE_TIMEOUT_MULTIPLIER, ADAPTIVE_TIMEOUT_PERCENTILE,
    CAPACITY_BACKOFF_THRESHOLD, CAPACITY_MIN_AT_CAP, CAPACITY_RAMP_THRESHOLD, CAPACITY_WINDOW_SIZE,
    CIRCUIT_TIMEOUT_SECONDS, MAX_CONCURRENT_REQUESTS, PERFORMANCE_MIN_SAMPLES,
    PERFORMANCE_RESCHEDULE_PENALTY, PERFORMANCE_SCORING_PERCENTILE, PERFORMANCE_WINDOW_SIZE,
};
use tracing::{info, warn};

pub struct PerformanceTracker {
    windows: HashMap<u16, VecDeque<(bool, f64)>>,
    adaptive_caps: HashMap<u16, usize>,
    at_cap_results: HashMap<u16, VecDeque<bool>>,
    window_size: usize,
    persistence_path: Option<PathBuf>,
}

impl PerformanceTracker {
    pub fn new_with_persistence(path: PathBuf) -> Self {
        let mut tracker = Self {
            windows: HashMap::new(),
            adaptive_caps: HashMap::new(),
            at_cap_results: HashMap::new(),
            window_size: PERFORMANCE_WINDOW_SIZE,
            persistence_path: Some(path),
        };
        tracker.load();
        tracker
    }

    pub fn record(&mut self, uid: u16, success: bool, response_time: f64, was_at_capacity: bool) {
        let window = self.windows.entry(uid).or_default();
        window.push_back((success, response_time));
        if window.len() > self.window_size {
            window.pop_front();
        }

        if was_at_capacity {
            let results = self.at_cap_results.entry(uid).or_default();
            results.push_back(success);
            if results.len() > CAPACITY_WINDOW_SIZE {
                results.pop_front();
            }
            self.update_adaptive_cap(uid);
        }
    }

    pub fn record_reschedule(&mut self, uid: u16) {
        let window = self.windows.entry(uid).or_default();
        window.push_back((false, PERFORMANCE_RESCHEDULE_PENALTY));
        if window.len() > self.window_size {
            window.pop_front();
        }
    }

    pub fn adaptive_timeout(&self) -> f64 {
        let times: Vec<f64> = self
            .windows
            .values()
            .flat_map(|w| w.iter().filter(|(s, _)| *s).map(|(_, t)| *t))
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

    pub fn save(&self) {
        let path = match &self.persistence_path {
            Some(p) => p,
            None => return,
        };

        let mut windows_json = serde_json::Map::new();
        for (uid, window) in &self.windows {
            let entries: Vec<serde_json::Value> = window
                .iter()
                .map(|(success, time)| serde_json::json!([*success, *time]))
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

        let tmp_path = path.with_extension("tmp");
        match serde_json::to_string(&data) {
            Ok(json_str) => {
                if let Err(e) = std::fs::write(&tmp_path, &json_str) {
                    warn!(error = %e, "writing performance tracker tmp file");
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp_path, path) {
                    warn!(error = %e, "renaming performance tracker file");
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

        let data = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => return,
        };

        let json: serde_json::Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "parsing performance tracker file");
                return;
            }
        };

        if let Some(windows) = json.get("windows").and_then(|v| v.as_object()) {
            for (uid_str, entries) in windows {
                let uid: u16 = match uid_str.parse() {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                if let Some(arr) = entries.as_array() {
                    let mut window = VecDeque::new();
                    for entry in arr {
                        if let Some(pair) = entry.as_array() {
                            if pair.len() == 2 {
                                let success = pair[0].as_bool().unwrap_or(false);
                                let time = pair[1].as_f64().unwrap_or(0.0);
                                window.push_back((success, time));
                            }
                        }
                    }
                    if !window.is_empty() {
                        self.windows.insert(uid, window);
                    }
                }
            }
        }

        if let Some(capacities) = json.get("capacities").and_then(|v| v.as_object()) {
            for (uid_str, cap_data) in capacities {
                let uid: u16 = match uid_str.parse() {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                if let Some(arr) = cap_data.as_array() {
                    if let Some(cap) = arr.first().and_then(|v| v.as_u64()) {
                        self.adaptive_caps.insert(uid, cap as usize);
                    }
                    if let Some(results) = arr.get(1).and_then(|v| v.as_array()) {
                        let mut deque = VecDeque::new();
                        for r in results {
                            deque.push_back(r.as_bool().unwrap_or(false));
                        }
                        if !deque.is_empty() {
                            self.at_cap_results.insert(uid, deque);
                        }
                    }
                }
            }
        }

        info!(
            windows = self.windows.len(),
            capacities = self.adaptive_caps.len(),
            "loaded performance tracker state"
        );
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
            .flat_map(|w| w.iter().filter(|(s, _)| *s).map(|(_, t)| *t))
            .collect();

        if times.len() < PERFORMANCE_MIN_SAMPLES {
            return CIRCUIT_TIMEOUT_SECONDS as f64;
        }

        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((times.len() as f64 * PERFORMANCE_SCORING_PERCENTILE) as usize)
            .min(times.len().saturating_sub(1));
        times[idx].max(1.0)
    }

    fn uid_rate(w: &VecDeque<(bool, f64)>, reference: f64) -> f64 {
        if w.is_empty() {
            return 0.0;
        }
        let mut total = 0.0;
        for &(success, rt) in w {
            if rt == PERFORMANCE_RESCHEDULE_PENALTY {
                total += PERFORMANCE_RESCHEDULE_PENALTY;
            } else if !success {
                // score = 0.0 for failures
            } else if rt <= 0.0 {
                total += 2.0;
            } else {
                total += (reference / rt).min(2.0);
            }
        }
        (total / w.len() as f64).max(0.0)
    }

    pub fn reset_uid(&mut self, uid: u16) {
        self.windows.remove(&uid);
        self.adaptive_caps.remove(&uid);
        self.at_cap_results.remove(&uid);
    }

    fn update_adaptive_cap(&mut self, uid: u16) {
        let success_rate = match self.at_cap_results.get(&uid) {
            Some(r) if r.len() >= CAPACITY_MIN_AT_CAP => {
                r.iter().filter(|&&s| s).count() as f64 / r.len() as f64
            }
            _ => return,
        };

        let current = self.adaptive_caps.entry(uid).or_insert(1);

        if success_rate >= CAPACITY_RAMP_THRESHOLD {
            *current = (*current + 1).min(MAX_CONCURRENT_REQUESTS);
            if let Some(r) = self.at_cap_results.get_mut(&uid) {
                r.clear();
            }
        } else if success_rate < CAPACITY_BACKOFF_THRESHOLD && *current > 1 {
            *current -= 1;
            if let Some(r) = self.at_cap_results.get_mut(&uid) {
                r.clear();
            }
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
            window_size: PERFORMANCE_WINDOW_SIZE,
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
        tracker.record(1, true, 5.0, false);
        tracker.record(1, true, 6.0, false);
        assert_eq!(tracker.windows.get(&1).unwrap().len(), 2);
    }

    #[test]
    fn record_reschedule_adds_penalty() {
        let mut tracker = test_tracker();
        tracker.record_reschedule(1);
        let window = tracker.windows.get(&1).unwrap();
        assert_eq!(window.len(), 1);
        let (success, time) = window[0];
        assert!(!success);
        assert_eq!(time, PERFORMANCE_RESCHEDULE_PENALTY);
    }

    #[test]
    fn reset_uid_clears_all_state() {
        let mut tracker = test_tracker();
        tracker.record(1, true, 5.0, true);
        tracker.adaptive_caps.insert(1, 4);
        tracker.reset_uid(1);
        assert!(!tracker.windows.contains_key(&1));
        assert!(!tracker.adaptive_caps.contains_key(&1));
        assert!(!tracker.at_cap_results.contains_key(&1));
    }

    #[test]
    fn adaptive_timeout_calculates_percentile_and_clamps() {
        let mut tracker = test_tracker();
        for i in 0..ADAPTIVE_TIMEOUT_MIN_SAMPLES {
            tracker.record(1, true, (i + 1) as f64, false);
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
            tracker2.record(1, true, 500.0, false);
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
        tracker.record(1, true, 5.0, false);
        let caps = tracker.miner_capacities();
        assert_eq!(caps.get(&1).copied().unwrap_or(0), 1);
    }
}
