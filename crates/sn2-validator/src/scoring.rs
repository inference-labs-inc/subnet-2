use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;
use tracing::{info, warn};

use sn2_types::{IP_REGION_CAP_FRACTION, PERFORMANCE_CURVE_POWER, PERFORMANCE_MIN_SAMPLES};

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

        let new_score = if verified {
            let distance = effective_max - previous_score;
            let change = rate_of_change * distance;
            previous_score + change
        } else {
            let distance = previous_score;
            let change = rate_of_change * distance;
            previous_score - change
        };

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
        sn2_types::atomic_write_json(&self.persistence_path, json.as_bytes())
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
        ip_regions: &HashMap<u16, String>,
        skiplisted: &HashSet<u16>,
        coldstart: &HashSet<u16>,
    ) -> (Vec<u16>, Vec<u16>) {
        let mut raw_weights: Vec<f64> = uids
            .iter()
            .map(|&uid| {
                if skiplisted.contains(&uid) || coldstart.contains(&uid) {
                    return 0.0;
                }
                let (delivered_work, _cap, count) = snap.get(&uid).copied().unwrap_or((0.0, 1, 0));
                if count >= PERFORMANCE_MIN_SAMPLES {
                    delivered_work.powf(PERFORMANCE_CURVE_POWER)
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

        apply_ip_region_cap(uids, &mut raw_weights, ip_regions);

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

pub fn ip_region(ip: &str) -> String {
    match ip.splitn(3, '.').collect::<Vec<_>>().as_slice() {
        [a, b, ..] if !ip.is_empty() => format!("{a}.{b}"),
        _ => String::new(),
    }
}

fn apply_ip_region_cap(uids: &[u16], weights: &mut [f64], ip_regions: &HashMap<u16, String>) {
    let total_miners = uids.len();
    if total_miners == 0 {
        return;
    }
    let max_per_region = ((total_miners as f64) * IP_REGION_CAP_FRACTION).floor() as usize;
    if max_per_region == 0 {
        return;
    }

    let mut region_indices: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, &uid) in uids.iter().enumerate() {
        let region = ip_regions.get(&uid).map(|s| s.as_str()).unwrap_or("");
        if !region.is_empty() {
            region_indices.entry(region).or_default().push(i);
        }
    }

    let mut zeroed_count = 0usize;
    for (region, indices) in &region_indices {
        if indices.len() <= max_per_region {
            continue;
        }
        let mut by_weight: Vec<usize> = indices.clone();
        by_weight.sort_by(|&a, &b| {
            weights[b]
                .partial_cmp(&weights[a])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for &idx in &by_weight[max_per_region..] {
            weights[idx] = 0.0;
            zeroed_count += 1;
        }
        info!(
            region = %region,
            miners_in_region = indices.len(),
            max_allowed = max_per_region,
            zeroed = by_weight.len() - max_per_region,
            "ip region cap applied"
        );
    }

    if zeroed_count > 0 {
        let new_total: f64 = weights.iter().sum();
        if new_total > 0.0 {
            for w in weights.iter_mut() {
                *w /= new_total;
            }
        }
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

    fn empty_regions() -> HashMap<u16, String> {
        HashMap::new()
    }

    fn empty_set() -> HashSet<u16> {
        HashSet::new()
    }

    fn regions_from(pairs: &[(u16, &str)]) -> HashMap<u16, String> {
        pairs.iter().map(|&(uid, r)| (uid, r.to_string())).collect()
    }

    #[test]
    fn compute_throughput_weights_normalizes_without_owner() {
        let mgr = test_manager();
        let mut snap = HashMap::new();
        snap.insert(1u16, (10.0, 2, PERFORMANCE_MIN_SAMPLES));
        snap.insert(2u16, (5.0, 2, PERFORMANCE_MIN_SAMPLES));
        let (uids, weights) = mgr.compute_throughput_weights(
            &[1, 2],
            &snap,
            None,
            &empty_regions(),
            &empty_set(),
            &empty_set(),
        );
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
        let (_, weights_no_owner) = mgr.compute_throughput_weights(
            &[1, 2, 3],
            &snap,
            None,
            &empty_regions(),
            &empty_set(),
            &empty_set(),
        );
        let (_, weights_with_owner) = mgr.compute_throughput_weights(
            &[1, 2, 3],
            &snap,
            Some(1),
            &empty_regions(),
            &empty_set(),
            &empty_set(),
        );
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
        let (_, weights) = mgr.compute_throughput_weights(
            &[1, 2],
            &snap,
            None,
            &empty_regions(),
            &empty_set(),
            &empty_set(),
        );
        assert_eq!(weights[0], 0);
        assert!(weights[1] > 0);
    }

    #[test]
    fn ip_region_cap_zeroes_excess_miners_in_saturated_region() {
        let mgr = test_manager();
        let uids: Vec<u16> = (0..8).collect();
        let mut snap = HashMap::new();
        for &uid in &uids {
            snap.insert(uid, (10.0, 1, PERFORMANCE_MIN_SAMPLES));
        }
        let regions = regions_from(&[
            (0, "10.0"),
            (1, "10.0"),
            (2, "10.0"),
            (3, "20.0"),
            (4, "30.0"),
            (5, "40.0"),
            (6, "50.0"),
            (7, "60.0"),
        ]);
        let (_, weights) = mgr.compute_throughput_weights(
            &uids,
            &snap,
            None,
            &regions,
            &empty_set(),
            &empty_set(),
        );
        let region_10_weights: Vec<u16> = vec![weights[0], weights[1], weights[2]];
        let nonzero = region_10_weights.iter().filter(|&&w| w > 0).count();
        assert_eq!(nonzero, 2);
    }

    #[test]
    fn ip_region_cap_keeps_top_performers() {
        let mgr = test_manager();
        let uids: Vec<u16> = (0..8).collect();
        let mut snap = HashMap::new();
        snap.insert(0u16, (20.0, 1, PERFORMANCE_MIN_SAMPLES));
        snap.insert(1u16, (5.0, 1, PERFORMANCE_MIN_SAMPLES));
        snap.insert(2u16, (10.0, 1, PERFORMANCE_MIN_SAMPLES));
        snap.insert(3u16, (1.0, 1, PERFORMANCE_MIN_SAMPLES));
        snap.insert(4u16, (10.0, 1, PERFORMANCE_MIN_SAMPLES));
        snap.insert(5u16, (10.0, 1, PERFORMANCE_MIN_SAMPLES));
        snap.insert(6u16, (10.0, 1, PERFORMANCE_MIN_SAMPLES));
        snap.insert(7u16, (10.0, 1, PERFORMANCE_MIN_SAMPLES));
        let regions = regions_from(&[
            (0, "10.0"),
            (1, "10.0"),
            (2, "10.0"),
            (3, "10.0"),
            (4, "20.0"),
            (5, "30.0"),
            (6, "40.0"),
            (7, "50.0"),
        ]);
        let (_, weights) = mgr.compute_throughput_weights(
            &uids,
            &snap,
            None,
            &regions,
            &empty_set(),
            &empty_set(),
        );
        assert!(weights[0] > 0, "top performer in region should keep weight");
        assert!(
            weights[2] > 0,
            "second performer in region should keep weight"
        );
        assert_eq!(
            weights[1], 0,
            "low performer in oversaturated region should be zeroed"
        );
        assert_eq!(
            weights[3], 0,
            "lowest performer in oversaturated region should be zeroed"
        );
    }

    #[test]
    fn ip_region_cap_no_effect_when_distributed() {
        let mgr = test_manager();
        let uids: Vec<u16> = (0..4).collect();
        let mut snap = HashMap::new();
        for &uid in &uids {
            snap.insert(uid, (10.0, 1, PERFORMANCE_MIN_SAMPLES));
        }
        let regions = regions_from(&[(0, "10.0"), (1, "20.0"), (2, "30.0"), (3, "40.0")]);
        let (_, weights_capped) = mgr.compute_throughput_weights(
            &uids,
            &snap,
            None,
            &regions,
            &empty_set(),
            &empty_set(),
        );
        let (_, weights_uncapped) = mgr.compute_throughput_weights(
            &uids,
            &snap,
            None,
            &empty_regions(),
            &empty_set(),
            &empty_set(),
        );
        assert_eq!(weights_capped, weights_uncapped);
    }

    #[test]
    fn ip_region_extraction() {
        assert_eq!(ip_region("192.168.1.100"), "192.168");
        assert_eq!(ip_region("10.0.0.1"), "10.0");
        assert_eq!(ip_region(""), "");
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
