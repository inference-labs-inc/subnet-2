use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result};
use tracing::{info, warn};

use sn2_types::{PERFORMANCE_CURVE_POWER, PERFORMANCE_MIN_SAMPLES};

const RATE_OF_DECAY: f64 = 0.4;
const RATE_OF_RECOVERY: f64 = 0.1;
const RESPONSE_TIME_WEIGHT: f64 = 1.0;
const MAXIMUM_RESPONSE_TIME_DECIMAL: f64 = 0.99;

pub struct ScoreManager {
    scores: HashMap<u16, f64>,
    persistence_path: PathBuf,
}

impl ScoreManager {
    pub fn new(persistence_path: PathBuf) -> Self {
        let mut mgr = Self {
            scores: HashMap::new(),
            persistence_path,
        };
        if let Err(e) = mgr.load() {
            warn!(error = %e, "no existing scores found, starting fresh");
        }
        mgr
    }

    pub fn get_score(&self, uid: u16) -> f64 {
        self.scores.get(&uid).copied().unwrap_or(0.0)
    }

    pub fn update_score(
        &mut self,
        uid: u16,
        verified: bool,
        response_time: f64,
        max_response_time: f64,
        min_response_time: f64,
        metagraph_n: u16,
    ) {
        let previous_score = self.get_score(uid);
        let maximum_score = 1.0 / (metagraph_n.max(1) as f64);

        let rate_of_change = if verified {
            RATE_OF_RECOVERY
        } else {
            RATE_OF_DECAY
        };

        let response_time_normalized = if max_response_time > min_response_time {
            let raw = (response_time - min_response_time) / (max_response_time - min_response_time);
            raw.clamp(0.0, MAXIMUM_RESPONSE_TIME_DECIMAL)
        } else {
            0.0
        };

        let response_time_metric =
            RESPONSE_TIME_WEIGHT * (1.0 - normalized_tangent_curve(response_time_normalized));

        let calculated_score_fraction = response_time_metric.clamp(0.0, 1.0);
        let effective_max = maximum_score * calculated_score_fraction;

        let (distance, new_score) = if verified {
            let distance = effective_max - previous_score;
            let change = rate_of_change * distance;
            (distance, previous_score + change)
        } else {
            let distance = previous_score;
            let change = rate_of_change * distance;
            (distance, previous_score - change)
        };

        let _ = distance;
        self.scores.insert(uid, new_score.max(0.0));
    }

    pub fn sync_uids(&mut self, active_uids: &[u16]) {
        self.scores.retain(|uid, _| active_uids.contains(uid));
        for &uid in active_uids {
            self.scores.entry(uid).or_insert(0.0);
        }
    }

    pub fn apply_pow_scores(&mut self, miner_uids: &[u16], scores: &[f64]) {
        if miner_uids.len() != scores.len() {
            warn!(
                miner_uids_len = miner_uids.len(),
                scores_len = scores.len(),
                "apply_pow_scores length mismatch, using minimum"
            );
        }
        for (&uid, &score) in miner_uids.iter().zip(scores.iter()) {
            if self.scores.contains_key(&uid) {
                self.scores.insert(uid, score.max(0.0));
            }
        }
    }

    pub fn zero_non_queryable(&mut self, queryable_uids: &HashSet<u16>) {
        for (uid, score) in self.scores.iter_mut() {
            if !queryable_uids.contains(uid) {
                *score = 0.0;
            }
        }
    }

    pub fn scores_snapshot(&self) -> &HashMap<u16, f64> {
        &self.scores
    }

    pub fn save(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.scores)?;
        std::fs::write(&self.persistence_path, json)
            .with_context(|| format!("writing scores to {}", self.persistence_path.display()))?;
        Ok(())
    }

    fn load(&mut self) -> Result<()> {
        let data = std::fs::read_to_string(&self.persistence_path)?;
        self.scores = serde_json::from_str(&data)?;
        info!(count = self.scores.len(), "loaded scores from disk");
        Ok(())
    }

    pub fn compute_throughput_weights(
        &self,
        uids: &[u16],
        snap: &HashMap<u16, (f64, usize, usize)>,
        owner_uid: Option<u16>,
    ) -> (Vec<u16>, Vec<u16>) {
        let mut raw_weights: Vec<f64> = uids
            .iter()
            .map(|&uid| {
                let (rate, cap, count) = snap.get(&uid).copied().unwrap_or((0.0, 1, 0));
                if count >= PERFORMANCE_MIN_SAMPLES {
                    let throughput = rate * cap as f64;
                    throughput.powf(PERFORMANCE_CURVE_POWER)
                } else {
                    0.0
                }
            })
            .collect();

        let total: f64 = raw_weights.iter().sum();
        if total > 0.0 {
            for w in &mut raw_weights {
                *w /= total;
            }
        }

        if let Some(owner) = owner_uid {
            if let Some(idx) = uids.iter().position(|&u| u == owner) {
                for w in &mut raw_weights {
                    *w *= 0.2;
                }
                raw_weights[idx] = 0.8;
            }
        }

        let weights: Vec<u16> = raw_weights
            .iter()
            .map(|&w| (w * u16::MAX as f64) as u16)
            .collect();

        (uids.to_vec(), weights)
    }
}

fn normalized_tangent_curve(x: f64) -> f64 {
    let shifted = x - 0.5;
    let scaled = shifted * std::f64::consts::PI * 0.9;
    (scaled.tan() / (std::f64::consts::PI * 0.45).tan() + 1.0) / 2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manager() -> ScoreManager {
        ScoreManager {
            scores: HashMap::new(),
            persistence_path: PathBuf::from("/dev/null"),
        }
    }

    #[test]
    fn default_score_is_zero() {
        let mgr = test_manager();
        assert_eq!(mgr.get_score(42), 0.0);
    }

    #[test]
    fn update_score_verified_increases() {
        let mut mgr = test_manager();
        mgr.update_score(1, true, 1.0, 2.0, 0.5, 100);
        assert!(mgr.get_score(1) > 0.0);
    }

    #[test]
    fn update_score_unverified_decreases() {
        let mut mgr = test_manager();
        mgr.scores.insert(1, 0.005);
        mgr.update_score(1, false, 1.0, 2.0, 0.5, 100);
        assert!(mgr.get_score(1) < 0.005);
    }

    #[test]
    fn score_never_negative() {
        let mut mgr = test_manager();
        mgr.scores.insert(1, 0.001);
        for _ in 0..100 {
            mgr.update_score(1, false, 1.0, 2.0, 0.5, 100);
        }
        assert!(mgr.get_score(1) >= 0.0);
    }

    #[test]
    fn sync_uids_removes_stale() {
        let mut mgr = test_manager();
        mgr.scores.insert(1, 0.5);
        mgr.scores.insert(2, 0.3);
        mgr.scores.insert(3, 0.1);
        mgr.sync_uids(&[1, 3]);
        assert!(mgr.scores.contains_key(&1));
        assert!(!mgr.scores.contains_key(&2));
        assert!(mgr.scores.contains_key(&3));
    }

    #[test]
    fn apply_pow_scores_updates_existing() {
        let mut mgr = test_manager();
        mgr.scores.insert(5, 0.0);
        mgr.scores.insert(10, 0.0);
        mgr.apply_pow_scores(&[5, 10, 42], &[0.8, 0.6, 0.9]);
        assert_eq!(mgr.get_score(5), 0.8);
        assert_eq!(mgr.get_score(10), 0.6);
        assert!(!mgr.scores.contains_key(&42));
    }

    #[test]
    fn compute_throughput_weights_normalizes_without_owner() {
        let mgr = test_manager();
        let mut snap = HashMap::new();
        snap.insert(1u16, (10.0, 2, PERFORMANCE_MIN_SAMPLES));
        snap.insert(2u16, (5.0, 2, PERFORMANCE_MIN_SAMPLES));
        let (uids, weights) = mgr.compute_throughput_weights(&[1, 2], &snap, None);
        assert_eq!(uids, vec![1, 2]);
        let total: u32 = weights.iter().map(|&w| w as u32).sum();
        assert!(total > 0);
        assert!(weights[0] > weights[1]);
    }

    #[test]
    fn compute_throughput_weights_owner_boost() {
        let mgr = test_manager();
        let mut snap = HashMap::new();
        snap.insert(1u16, (10.0, 1, PERFORMANCE_MIN_SAMPLES));
        snap.insert(2u16, (10.0, 1, PERFORMANCE_MIN_SAMPLES));
        snap.insert(3u16, (10.0, 1, PERFORMANCE_MIN_SAMPLES));
        let (_, weights_no_owner) = mgr.compute_throughput_weights(&[1, 2, 3], &snap, None);
        let (_, weights_with_owner) = mgr.compute_throughput_weights(&[1, 2, 3], &snap, Some(1));
        assert!(weights_with_owner[0] > weights_no_owner[0]);
        let owner_ratio = weights_with_owner[0] as f64 / u16::MAX as f64;
        assert!((owner_ratio - 0.8).abs() < 0.01);
    }

    #[test]
    fn compute_throughput_weights_below_min_samples_is_zero() {
        let mgr = test_manager();
        let mut snap = HashMap::new();
        snap.insert(1u16, (10.0, 1, PERFORMANCE_MIN_SAMPLES - 1));
        snap.insert(2u16, (10.0, 1, PERFORMANCE_MIN_SAMPLES));
        let (_, weights) = mgr.compute_throughput_weights(&[1, 2], &snap, None);
        assert_eq!(weights[0], 0);
        assert!(weights[1] > 0);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "sn2_score_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let path = dir.join("scores.json");
        std::fs::create_dir_all(&dir).unwrap();

        let mut mgr = ScoreManager {
            scores: HashMap::new(),
            persistence_path: path.clone(),
        };
        mgr.scores.insert(1, 0.5);
        mgr.scores.insert(2, 0.3);
        mgr.save().unwrap();

        let mut loaded = ScoreManager {
            scores: HashMap::new(),
            persistence_path: path.clone(),
        };
        loaded.load().unwrap();
        assert_eq!(loaded.scores.len(), 2);
        assert_eq!(loaded.get_score(1), 0.5);
        assert_eq!(loaded.get_score(2), 0.3);

        std::fs::remove_dir_all(&dir).expect("failed to remove temp dir after round-trip test");
    }

    #[test]
    fn zero_non_queryable() {
        let mut mgr = test_manager();
        mgr.scores.insert(1, 0.5);
        mgr.scores.insert(2, 0.3);
        let queryable: HashSet<u16> = [1].into_iter().collect();
        mgr.zero_non_queryable(&queryable);
        assert_eq!(mgr.get_score(1), 0.5);
        assert_eq!(mgr.get_score(2), 0.0);
    }
}
