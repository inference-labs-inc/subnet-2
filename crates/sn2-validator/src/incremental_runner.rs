use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::tensor::arrayd_to_json;
use dsperse::pipeline::{CombinedRun, SliceWork};
use dsperse::schema::tiling::TilingInfo;
use sn2_types::{BoundedFifoSet, ProofSystem, RunSource};
use tracing::{debug, info, warn};

pub enum TileBufferOutcome {
    Waiting,
    AllReceived,
    Failed(String),
}

#[derive(Debug)]
pub enum OutputConsistency {
    Consistent { max_rel_err: f64 },
    Diverged { max_rel_err: f64 },
    LengthMismatch { expected: usize, actual: usize },
    NoExpected,
    NoRun,
}

const OUTPUT_CONSISTENCY_THRESHOLD: f64 = 0.05;

pub fn classify_output_consistency(expected: &[f64], actual: &[f64]) -> OutputConsistency {
    if expected.len() != actual.len() {
        return OutputConsistency::LengthMismatch {
            expected: expected.len(),
            actual: actual.len(),
        };
    }
    let mut max_rel_err: f64 = 0.0;
    for (e, m) in expected.iter().zip(actual.iter()) {
        if !e.is_finite() || !m.is_finite() {
            return OutputConsistency::Diverged {
                max_rel_err: f64::INFINITY,
            };
        }
        let denom = e.abs().max(1e-12);
        let rel = (e - m).abs() / denom;
        if rel > max_rel_err {
            max_rel_err = rel;
        }
    }
    if max_rel_err > OUTPUT_CONSISTENCY_THRESHOLD {
        OutputConsistency::Diverged { max_rel_err }
    } else {
        OutputConsistency::Consistent { max_rel_err }
    }
}

#[allow(dead_code)]
pub struct SliceArtifact {
    pub slice_num: String,
    pub proof_system: Option<ProofSystem>,
    pub proof_hex: Option<String>,
    pub witness_hex: Option<String>,
    pub computed_outputs: Option<serde_json::Value>,
    pub tile_idx: Option<u32>,
    pub response_time: f64,
    pub verification_time: f64,
}

#[allow(dead_code)]
pub struct ActiveRun {
    pub run_uid: String,
    pub circuit_id: String,
    pub circuit_name: String,
    pub run_source: RunSource,
    pub started_at: Instant,
    pub last_activity: Instant,
    pub artifacts: Vec<SliceArtifact>,
    pub relay_request_id: Option<u32>,
    pub combined: Option<CombinedRun>,
}

struct TileCounter {
    total: usize,
    received: HashSet<u32>,
}

const EVICTED_CAP: usize = 256;

pub struct IncrementalRunManager {
    runs: HashMap<String, ActiveRun>,
    evicted: BoundedFifoSet<String>,
    tile_counters: HashMap<(String, String), TileCounter>,
}

impl Default for IncrementalRunManager {
    fn default() -> Self {
        Self {
            runs: HashMap::new(),
            evicted: BoundedFifoSet::new(EVICTED_CAP),
            tile_counters: HashMap::new(),
        }
    }
}

impl IncrementalRunManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start_run(
        &mut self,
        run_uid: String,
        circuit_id: String,
        circuit_name: String,
        run_source: RunSource,
        relay_request_id: Option<u32>,
        combined: Option<CombinedRun>,
    ) {
        if self.runs.contains_key(&run_uid) {
            warn!(
                run_uid = %run_uid,
                relay_request_id = ?relay_request_id,
                "duplicate start_run for existing ActiveRun, skipping"
            );
            return;
        }
        let now = Instant::now();
        self.runs.insert(
            run_uid.clone(),
            ActiveRun {
                run_uid,
                circuit_id,
                circuit_name,
                run_source,
                started_at: now,
                last_activity: now,
                artifacts: Vec::new(),
                relay_request_id,
                combined,
            },
        );
    }

    pub fn has_run(&self, run_uid: &str) -> bool {
        self.runs.contains_key(run_uid)
    }

    pub fn slice_tile_counts(&self, run_uid: &str) -> (usize, usize, HashMap<String, usize>) {
        let run = match self.runs.get(run_uid) {
            Some(r) => r,
            None => return (0, 0, HashMap::new()),
        };
        match run.combined.as_ref() {
            Some(c) => c.slice_tile_counts(),
            None => (0, 0, HashMap::new()),
        }
    }

    pub fn verify_output_consistency(
        &self,
        run_uid: &str,
        slice_id: &str,
        miner_outputs: &[f64],
    ) -> OutputConsistency {
        let run = match self.runs.get(run_uid) {
            Some(r) => r,
            None => return OutputConsistency::NoRun,
        };
        let expected = match run
            .combined
            .as_ref()
            .and_then(|c| c.expected_slice_outputs(slice_id))
        {
            Some(e) => e,
            None => return OutputConsistency::NoExpected,
        };
        classify_output_consistency(&expected, miner_outputs)
    }

    pub fn is_evicted(&self, run_uid: &str) -> bool {
        self.evicted.contains(run_uid)
    }

    pub fn get_run_source(&self, run_uid: &str) -> Option<RunSource> {
        self.runs.get(run_uid).map(|r| r.run_source)
    }

    pub fn all_circuit_work(&self, run_uid: &str) -> anyhow::Result<Vec<SliceWork>> {
        let run = self
            .runs
            .get(run_uid)
            .ok_or_else(|| anyhow::anyhow!("unknown run {run_uid}"))?;
        let combined = run
            .combined
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("run {run_uid} has no CombinedRun"))?;
        combined
            .all_circuit_work()
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    pub fn init_tile_counter(
        &mut self,
        run_uid: &str,
        slice_id: &str,
        tiling: &TilingInfo,
    ) -> anyhow::Result<()> {
        let expected = tiling.tiles_y * tiling.tiles_x;
        if tiling.num_tiles != expected {
            return Err(anyhow::anyhow!(
                "TilingInfo.num_tiles inconsistent for run {run_uid}, slice {slice_id}: num_tiles={}, tiles_y*tiles_x={expected}",
                tiling.num_tiles,
            ));
        }
        info!(
            run_uid = %run_uid,
            slice = %slice_id,
            num_tiles = tiling.num_tiles,
            "initialized tile counter"
        );
        let key = (run_uid.to_string(), slice_id.to_string());
        use std::collections::hash_map::Entry;
        match self.tile_counters.entry(key) {
            Entry::Vacant(e) => {
                e.insert(TileCounter {
                    total: tiling.num_tiles,
                    received: HashSet::with_capacity(tiling.num_tiles),
                });
            }
            Entry::Occupied(_) => {
                return Err(anyhow::anyhow!(
                    "tile counter already exists for run {run_uid}, slice {slice_id}"
                ));
            }
        }
        Ok(())
    }

    pub fn record_tile(
        &mut self,
        run_uid: &str,
        slice_id: &str,
        tile_idx: u32,
    ) -> TileBufferOutcome {
        if let Some(run) = self.runs.get_mut(run_uid) {
            run.last_activity = Instant::now();
        }
        let key = (run_uid.to_string(), slice_id.to_string());
        let counter = match self.tile_counters.get_mut(&key) {
            Some(c) => c,
            None => {
                debug!(
                    run_uid = %run_uid,
                    slice = %slice_id,
                    tile_idx,
                    "tile counter absent, late/duplicate tile after slice completion"
                );
                return TileBufferOutcome::Waiting;
            }
        };

        if (tile_idx as usize) >= counter.total {
            return TileBufferOutcome::Failed(format!(
                "tile_idx {tile_idx} out of range (expected < {}) for run={run_uid} slice={slice_id}",
                counter.total
            ));
        }

        if !counter.received.insert(tile_idx) {
            debug!(
                run_uid = %run_uid,
                slice = %slice_id,
                tile_idx = tile_idx,
                "duplicate tile proof, ignoring"
            );
            return TileBufferOutcome::Waiting;
        }
        debug!(
            run_uid = %run_uid,
            slice = %slice_id,
            tile_idx = tile_idx,
            received = counter.received.len(),
            total = counter.total,
            "recorded tile proof"
        );

        if counter.received.len() < counter.total {
            return TileBufferOutcome::Waiting;
        }

        self.tile_counters.remove(&key);
        info!(
            run_uid = %run_uid,
            slice = %slice_id,
            "all tile proofs received"
        );
        TileBufferOutcome::AllReceived
    }

    pub fn mark_slice_done(&mut self, run_uid: &str, slice_id: &str) -> bool {
        if let Some(run) = self.runs.get_mut(run_uid) {
            run.last_activity = Instant::now();
            if let Some(ref mut combined) = run.combined {
                if !combined.mark_slice_done(slice_id) {
                    warn!(
                        run_uid = %run_uid,
                        slice = %slice_id,
                        "mark_slice_done called for unknown or already-completed slice"
                    );
                    return false;
                }
                return true;
            }
        }
        false
    }

    pub fn is_run_complete(&self, run_uid: &str) -> bool {
        self.runs
            .get(run_uid)
            .and_then(|r| r.combined.as_ref())
            .is_some_and(|c| c.is_complete())
    }

    pub fn final_output_json(&self, run_uid: &str) -> Option<serde_json::Value> {
        let run = self.runs.get(run_uid)?;
        let combined = run.combined.as_ref()?;
        let output = combined.final_output()?;
        Some(arrayd_to_json(output))
    }

    pub fn push_artifact(&mut self, run_uid: &str, artifact: SliceArtifact) {
        if let Some(run) = self.runs.get_mut(run_uid) {
            run.artifacts.push(artifact);
        }
    }

    pub fn remove_run(&mut self, run_uid: &str) -> Option<ActiveRun> {
        self.tile_counters.retain(|(uid, _), _| uid != run_uid);
        self.runs.remove(run_uid)
    }

    pub fn active_count(&self) -> usize {
        self.runs.len()
    }

    pub fn has_benchmark_runs(&self) -> bool {
        self.runs
            .values()
            .any(|r| r.run_source == RunSource::Benchmark)
    }

    pub fn benchmark_run_uids(&self) -> Vec<String> {
        self.runs
            .iter()
            .filter(|(_, run)| run.run_source == RunSource::Benchmark)
            .map(|(uid, _)| uid.clone())
            .collect()
    }

    pub fn evict_by_circuit(&mut self, circuit_id: &str) -> Vec<String> {
        let to_remove: Vec<String> = self
            .runs
            .iter()
            .filter(|(_, run)| run.circuit_id == circuit_id)
            .map(|(uid, _)| uid.clone())
            .collect();
        let evict_set: HashSet<&str> = to_remove.iter().map(|s| s.as_str()).collect();
        self.tile_counters
            .retain(|(run_uid, _), _| !evict_set.contains(run_uid.as_str()));
        for uid in to_remove.iter() {
            self.runs.remove(uid);
            self.evicted.insert(uid.clone());
        }
        to_remove
    }

    pub fn gc_stale(&mut self, idle_timeout: Duration) -> Vec<String> {
        let now = Instant::now();
        let stale: Vec<String> = self
            .runs
            .iter()
            .filter(|(_, run)| now.duration_since(run.last_activity) >= idle_timeout)
            .map(|(uid, _)| uid.clone())
            .collect();
        if !stale.is_empty() {
            info!(count = stale.len(), run_uids = ?stale, "evicting idle runs");
        }
        let stale_set: HashSet<&str> = stale.iter().map(|s| s.as_str()).collect();
        self.tile_counters
            .retain(|(run_uid, _), _| !stale_set.contains(run_uid.as_str()));
        for uid in stale.iter() {
            self.runs.remove(uid);
            self.evicted.insert(uid.clone());
        }
        stale
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manager_with_run(run_uid: &str) -> IncrementalRunManager {
        let mut mgr = IncrementalRunManager::new();
        mgr.start_run(
            run_uid.to_string(),
            "test-circuit".to_string(),
            "test".to_string(),
            RunSource::Benchmark,
            None,
            None,
        );
        mgr
    }

    #[test]
    fn output_consistency_no_run() {
        let mgr = IncrementalRunManager::new();
        let result = mgr.verify_output_consistency("nonexistent", "slice_0", &[1.0, 2.0]);
        assert!(matches!(result, OutputConsistency::NoRun));
    }

    #[test]
    fn output_consistency_no_combined() {
        let mgr = make_manager_with_run("run-1");
        let result = mgr.verify_output_consistency("run-1", "slice_0", &[1.0, 2.0]);
        assert!(matches!(result, OutputConsistency::NoExpected));
    }

    #[test]
    fn output_consistency_length_mismatch_detected() {
        let result = classify_output_consistency(&[1.0, 2.0, 3.0], &[1.0, 2.0]);
        assert!(matches!(
            result,
            OutputConsistency::LengthMismatch {
                expected: 3,
                actual: 2
            }
        ));
    }

    #[test]
    fn output_consistency_exact_match() {
        let vals = [1.0, 2.0, 3.0];
        let result = classify_output_consistency(&vals, &vals);
        assert!(
            matches!(result, OutputConsistency::Consistent { max_rel_err } if max_rel_err == 0.0)
        );
    }

    #[test]
    fn output_consistency_within_threshold() {
        let expected = [1.0, 2.0, 3.0];
        let perturbed: Vec<f64> = expected.iter().map(|v| v * 1.01).collect();
        let result = classify_output_consistency(&expected, &perturbed);
        assert!(
            matches!(result, OutputConsistency::Consistent { .. }),
            "1% perturbation should be within threshold, got {result:?}"
        );
    }

    #[test]
    fn output_consistency_forgery_detected() {
        let result = classify_output_consistency(&[1.0, 2.0, 3.0], &[5.0, 10.0, 15.0]);
        assert!(
            matches!(result, OutputConsistency::Diverged { .. }),
            "completely different outputs should be detected, got {result:?}"
        );
    }

    #[test]
    fn output_consistency_wrong_weights_detected() {
        let result = classify_output_consistency(&[0.95, 0.03, 0.02], &[0.40, 0.35, 0.25]);
        assert!(
            matches!(result, OutputConsistency::Diverged { .. }),
            "outputs from wrong weights should be detected, got {result:?}"
        );
    }

    #[test]
    fn output_consistency_near_zero_stability() {
        let result = classify_output_consistency(&[1e-15, 0.0, -1e-15], &[0.0, 0.0, 0.0]);
        assert!(
            matches!(result, OutputConsistency::Consistent { .. }),
            "near-zero values should not trigger false positives, got {result:?}"
        );
    }

    #[test]
    fn output_consistency_nan_detected() {
        let result = classify_output_consistency(&[1.0, f64::NAN, 3.0], &[1.0, 2.0, 3.0]);
        assert!(
            matches!(result, OutputConsistency::Diverged { max_rel_err } if max_rel_err.is_infinite())
        );
    }

    #[test]
    fn output_consistency_inf_detected() {
        let result = classify_output_consistency(&[1.0, 2.0, 3.0], &[1.0, f64::INFINITY, 3.0]);
        assert!(
            matches!(result, OutputConsistency::Diverged { max_rel_err } if max_rel_err.is_infinite())
        );
    }
}
