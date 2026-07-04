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

const OUTPUT_CONSISTENCY_THRESHOLD: f64 = 0.10;

pub fn group_expected_region(
    arrays: &[ndarray::ArrayD<f64>],
    concat_axis: usize,
    num_groups: usize,
    tile_idx: u32,
) -> Option<Vec<f64>> {
    if num_groups == 0 {
        return None;
    }
    let mut flat = Vec::new();
    for arr in arrays {
        if concat_axis >= arr.ndim() || arr.shape()[concat_axis] % num_groups != 0 {
            return None;
        }
        let extent = arr.shape()[concat_axis] / num_groups;
        let start = tile_idx as usize * extent;
        if start + extent > arr.shape()[concat_axis] {
            return None;
        }
        let region = arr.slice_axis(
            ndarray::Axis(concat_axis),
            ndarray::Slice::from(start..start + extent),
        );
        flat.extend(region.as_standard_layout().iter());
    }
    if flat.is_empty() {
        None
    } else {
        Some(flat)
    }
}

pub fn classify_output_consistency(expected: &[f64], actual: &[f64]) -> OutputConsistency {
    if expected.is_empty() || actual.is_empty() {
        return OutputConsistency::LengthMismatch {
            expected: expected.len(),
            actual: actual.len(),
        };
    }
    let compare_len = expected.len().min(actual.len());
    let expected = &expected[..compare_len];
    let actual = &actual[..compare_len];
    if expected.iter().chain(actual.iter()).any(|v| !v.is_finite()) {
        return OutputConsistency::Diverged {
            max_rel_err: f64::INFINITY,
        };
    }
    let dot: f64 = expected.iter().zip(actual.iter()).map(|(e, m)| e * m).sum();
    let norm_sq: f64 = actual.iter().map(|m| m * m).sum();
    let scale = if norm_sq > 1e-24 { dot / norm_sq } else { 0.0 };
    let scale = if scale.is_finite() && scale.abs() > 1e-12 {
        scale
    } else {
        1.0
    };
    let magnitude: f64 = expected.iter().map(|e| e.abs()).sum::<f64>() / compare_len as f64;
    let mut max_rel_err: f64 = 0.0;
    for (e, m) in expected.iter().zip(actual.iter()) {
        let denom = e.abs().max(magnitude).max(1e-12);
        let rel = (e - scale * m).abs() / denom;
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
    grid_total: usize,
    expected: HashSet<u32>,
    received: HashSet<u32>,
}

const EVICTED_CAP: usize = 256;

pub struct IncrementalRunManager {
    runs: HashMap<String, ActiveRun>,
    evicted: BoundedFifoSet<String>,
    tile_counters: HashMap<(String, String), TileCounter>,
    verified_tile_counts: HashMap<(String, String), usize>,
    // Slices marked failed without ever being dispatched (e.g. previously
    // disabled, or rejected by preflight). Tracked per run so the run-wide
    // failure guard in finalize_combined_run can distinguish "no slice was
    // attempted" from "every attempted slice failed".
    skipped_slices: HashMap<String, HashSet<String>>,
}

impl Default for IncrementalRunManager {
    fn default() -> Self {
        Self {
            runs: HashMap::new(),
            evicted: BoundedFifoSet::new(EVICTED_CAP),
            tile_counters: HashMap::new(),
            verified_tile_counts: HashMap::new(),
            skipped_slices: HashMap::new(),
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

    pub fn circuit_id_for_run(&self, run_uid: &str) -> Option<&str> {
        self.runs.get(run_uid).map(|r| r.circuit_id.as_str())
    }

    pub fn verified_tile_count(&self, run_uid: &str, slice_id: &str) -> usize {
        self.verified_tile_counts
            .get(&(run_uid.to_string(), slice_id.to_string()))
            .copied()
            .unwrap_or(0)
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
        miner_outputs: &[f64],
        input_norm_factor: Option<f64>,
        circuit_output_names: &[String],
        group_tile: Option<(&dsperse::schema::tiling::DimSplitInfo, u32)>,
    ) -> OutputConsistency {
        let run = match self.runs.get(run_uid) {
            Some(r) => r,
            None => return OutputConsistency::NoRun,
        };
        let combined = match run.combined.as_ref() {
            Some(c) => c,
            None => return OutputConsistency::NoExpected,
        };
        let expected = match group_tile {
            Some((ds, tile_idx)) => {
                let arrays = match combined.output_arrays_for_names(circuit_output_names) {
                    Some(a) => a,
                    None => return OutputConsistency::NoExpected,
                };
                match group_expected_region(&arrays, ds.concat_axis, ds.num_groups, tile_idx) {
                    Some(flat) => flat,
                    None => return OutputConsistency::NoExpected,
                }
            }
            None => match combined.outputs_for_names(circuit_output_names) {
                Some(e) => e,
                None => return OutputConsistency::NoExpected,
            },
        };
        match input_norm_factor {
            Some(k) if k > 1.0 => {
                let normalized: Vec<f64> = expected.iter().map(|v| v / k).collect();
                classify_output_consistency(&normalized, miner_outputs)
            }
            _ => classify_output_consistency(&expected, miner_outputs),
        }
    }

    pub fn group_dim_split_meta(
        &self,
        run_uid: &str,
        slice_id: &str,
    ) -> Option<dsperse::schema::tiling::DimSplitInfo> {
        let run = self.runs.get(run_uid)?;
        let combined = run.combined.as_ref()?;
        let index: usize = slice_id.strip_prefix("slice_")?.parse().ok()?;
        combined
            .model_meta()
            .slices
            .iter()
            .find(|s| s.index == index)
            .and_then(|s| s.dim_split.clone())
            .filter(|ds| ds.weight_name.is_none())
    }

    pub fn is_evicted(&self, run_uid: &str) -> bool {
        self.evicted.contains(run_uid)
    }

    pub fn circuit_work_ids(&self, run_uid: &str) -> anyhow::Result<Vec<String>> {
        Ok(self.combined_for(run_uid)?.circuit_work_ids())
    }

    pub fn circuit_work_for(&self, run_uid: &str, slice_id: &str) -> anyhow::Result<SliceWork> {
        self.combined_for(run_uid)?
            .circuit_work_for(slice_id)
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn combined_for(&self, run_uid: &str) -> anyhow::Result<&CombinedRun> {
        let run = self
            .runs
            .get(run_uid)
            .ok_or_else(|| anyhow::anyhow!("unknown run {run_uid}"))?;
        run.combined
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("run {run_uid} has no CombinedRun"))
    }

    pub fn init_tile_counter(
        &mut self,
        run_uid: &str,
        slice_id: &str,
        tiling: &TilingInfo,
        expected_indices: HashSet<u32>,
    ) -> anyhow::Result<()> {
        let grid_total = tiling.tiles_y * tiling.tiles_x;
        if tiling.num_tiles != grid_total {
            return Err(anyhow::anyhow!(
                "TilingInfo.num_tiles inconsistent for run {run_uid}, slice {slice_id}: num_tiles={}, tiles_y*tiles_x={grid_total}",
                tiling.num_tiles,
            ));
        }
        if expected_indices.is_empty() {
            return Err(anyhow::anyhow!(
                "tile counter init requires at least one expected tile for run {run_uid}, slice {slice_id}"
            ));
        }
        if let Some(&max_idx) = expected_indices.iter().max() {
            if (max_idx as usize) >= tiling.num_tiles {
                return Err(anyhow::anyhow!(
                    "expected tile index {max_idx} exceeds tiling.num_tiles {} for run {run_uid}, slice {slice_id}",
                    tiling.num_tiles,
                ));
            }
        }
        let expected_count = expected_indices.len();
        info!(
            run_uid = %run_uid,
            slice = %slice_id,
            num_tiles = tiling.num_tiles,
            expected = expected_count,
            "initialized tile counter"
        );
        let key = (run_uid.to_string(), slice_id.to_string());
        use std::collections::hash_map::Entry;
        match self.tile_counters.entry(key) {
            Entry::Vacant(e) => {
                e.insert(TileCounter {
                    grid_total: tiling.num_tiles,
                    expected: expected_indices,
                    received: HashSet::with_capacity(expected_count),
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

    pub fn init_dim_split_counter(
        &mut self,
        run_uid: &str,
        slice_id: &str,
        total_units: usize,
        expected_indices: HashSet<u32>,
    ) -> anyhow::Result<()> {
        if total_units == 0 {
            return Err(anyhow::anyhow!(
                "dim-split counter init requires total_units > 0 for run {run_uid}, slice {slice_id}"
            ));
        }
        if expected_indices.is_empty() {
            return Err(anyhow::anyhow!(
                "dim-split counter init requires at least one expected unit for run {run_uid}, slice {slice_id}"
            ));
        }
        if let Some(&max_idx) = expected_indices.iter().max() {
            if (max_idx as usize) >= total_units {
                return Err(anyhow::anyhow!(
                    "expected unit index {max_idx} exceeds total_units {total_units} for run {run_uid}, slice {slice_id}"
                ));
            }
        }
        let expected_count = expected_indices.len();
        let key = (run_uid.to_string(), slice_id.to_string());
        use std::collections::hash_map::Entry;
        match self.tile_counters.entry(key) {
            Entry::Vacant(e) => {
                e.insert(TileCounter {
                    grid_total: total_units,
                    expected: expected_indices,
                    received: HashSet::with_capacity(expected_count),
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

        if (tile_idx as usize) >= counter.grid_total {
            return TileBufferOutcome::Failed(format!(
                "tile_idx {tile_idx} out of range (grid_total {}) for run={run_uid} slice={slice_id}",
                counter.grid_total
            ));
        }

        if !counter.expected.contains(&tile_idx) {
            debug!(
                run_uid = %run_uid,
                slice = %slice_id,
                tile_idx,
                expected = counter.expected.len(),
                "tile proof for non-sampled tile_idx, ignoring"
            );
            return TileBufferOutcome::Waiting;
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
        *self.verified_tile_counts.entry(key.clone()).or_insert(0) += 1;
        debug!(
            run_uid = %run_uid,
            slice = %slice_id,
            tile_idx = tile_idx,
            received = counter.received.len(),
            expected = counter.expected.len(),
            "recorded tile proof"
        );

        if counter.received.len() < counter.expected.len() {
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

    pub fn mark_slice_failed(&mut self, run_uid: &str, slice_id: &str) -> usize {
        self.tile_counters
            .remove(&(run_uid.to_string(), slice_id.to_string()));
        if let Some(run) = self.runs.get_mut(run_uid) {
            run.last_activity = Instant::now();
            if let Some(ref mut combined) = run.combined {
                combined.mark_slice_failed(slice_id);
                return combined.failed_count();
            }
        }
        0
    }

    pub fn is_slice_failed(&self, run_uid: &str, slice_id: &str) -> bool {
        self.runs
            .get(run_uid)
            .and_then(|r| r.combined.as_ref())
            .is_some_and(|c| c.is_slice_failed(slice_id))
    }

    pub fn failed_slice_count(&self, run_uid: &str) -> usize {
        self.runs
            .get(run_uid)
            .and_then(|r| r.combined.as_ref())
            .map(|c| c.failed_count())
            .unwrap_or(0)
    }

    /// Record that a slice was marked failed without ever being dispatched.
    /// Used by the run-wide failure guard in finalize_combined_run to avoid
    /// conflating deterministic skips (already-disabled slices, preflight
    /// rejections) with the "every attempted slice failed" signal that
    /// triggers the disable-list write-suppression.
    pub fn note_slice_skipped(&mut self, run_uid: &str, slice_id: &str) {
        self.skipped_slices
            .entry(run_uid.to_string())
            .or_default()
            .insert(slice_id.to_string());
    }

    pub fn skipped_slice_count(&self, run_uid: &str) -> usize {
        self.skipped_slices
            .get(run_uid)
            .map(HashSet::len)
            .unwrap_or(0)
    }

    /// Returns true when the slice was marked failed without ever being
    /// dispatched in this run (already-disabled, preflight rejection, etc.).
    /// finalize_combined_run uses this to keep skipped slices out of the
    /// disable-list write — they were never attempted, so resetting their
    /// disabled_at block would defeat the rehab cooldown.
    pub fn is_slice_skipped(&self, run_uid: &str, slice_id: &str) -> bool {
        self.skipped_slices
            .get(run_uid)
            .is_some_and(|s| s.contains(slice_id))
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
        self.verified_tile_counts
            .retain(|(uid, _), _| uid != run_uid);
        self.skipped_slices.remove(run_uid);
        self.runs.remove(run_uid)
    }

    pub fn active_count(&self) -> usize {
        self.runs.len()
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
        self.verified_tile_counts
            .retain(|(run_uid, _), _| !evict_set.contains(run_uid.as_str()));
        self.skipped_slices
            .retain(|run_uid, _| !evict_set.contains(run_uid.as_str()));
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
        self.verified_tile_counts
            .retain(|(run_uid, _), _| !stale_set.contains(run_uid.as_str()));
        self.skipped_slices
            .retain(|run_uid, _| !stale_set.contains(run_uid.as_str()));
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

    #[test]
    fn group_region_slices_each_output_along_concat_axis() {
        let a = ndarray::ArrayD::from_shape_vec(
            ndarray::IxDyn(&[2, 4, 3]),
            (0..24).map(|v| v as f64).collect(),
        )
        .unwrap();
        let b = ndarray::ArrayD::from_shape_vec(
            ndarray::IxDyn(&[1, 4, 2]),
            (100..108).map(|v| v as f64).collect(),
        )
        .unwrap();
        let region = group_expected_region(&[a.clone(), b.clone()], 1, 2, 1).unwrap();
        assert_eq!(region.len(), 12 + 4);
        assert_eq!(region[0], 6.0);
        assert_eq!(region[12], 104.0);
        let full: Vec<f64> = group_expected_region(&[a.clone(), b.clone()], 1, 2, 0)
            .unwrap()
            .into_iter()
            .chain(group_expected_region(&[a, b], 1, 2, 1).unwrap())
            .collect();
        assert_eq!(full.len(), 32);
    }

    #[test]
    fn group_region_rejects_unmappable_shapes() {
        let a = ndarray::ArrayD::from_shape_vec(
            ndarray::IxDyn(&[2, 5, 3]),
            (0..30).map(|v| v as f64).collect(),
        )
        .unwrap();
        assert!(group_expected_region(std::slice::from_ref(&a), 1, 2, 0).is_none());
        assert!(group_expected_region(std::slice::from_ref(&a), 9, 2, 0).is_none());
        assert!(group_expected_region(&[a], 1, 0, 0).is_none());
        let b = ndarray::ArrayD::from_shape_vec(
            ndarray::IxDyn(&[2, 4, 3]),
            (0..24).map(|v| v as f64).collect(),
        )
        .unwrap();
        assert!(group_expected_region(&[b], 1, 2, 5).is_none());
    }

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
        let result = mgr.verify_output_consistency("nonexistent", &[1.0, 2.0], None, &[], None);
        assert!(matches!(result, OutputConsistency::NoRun));
    }

    #[test]
    fn output_consistency_no_combined() {
        let mgr = make_manager_with_run("run-1");
        let result = mgr.verify_output_consistency("run-1", &[1.0, 2.0], None, &[], None);
        assert!(matches!(result, OutputConsistency::NoExpected));
    }

    #[test]
    fn output_consistency_truncates_to_shorter() {
        let result = classify_output_consistency(&[1.0, 2.0, 3.0], &[1.0, 2.0]);
        assert!(
            matches!(result, OutputConsistency::Consistent { .. }),
            "matching prefix with different lengths should be consistent, got {result:?}"
        );
    }

    #[test]
    fn output_consistency_empty_is_mismatch() {
        let result = classify_output_consistency(&[1.0], &[]);
        assert!(matches!(result, OutputConsistency::LengthMismatch { .. }));
        let result = classify_output_consistency(&[], &[1.0]);
        assert!(matches!(result, OutputConsistency::LengthMismatch { .. }));
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
    fn output_consistency_tolerates_uniform_scaling() {
        let result = classify_output_consistency(&[1.0, 2.0, 3.0], &[5.0, 10.0, 15.0]);
        assert!(
            matches!(result, OutputConsistency::Consistent { .. }),
            "scalar multiples reflect normalization conventions, got {result:?}"
        );
    }

    #[test]
    fn output_consistency_reordered_outputs_detected() {
        let expected = [0.9, -0.4, 0.05, 0.7, -0.2, 0.33, -0.8, 0.12];
        let shuffled = [0.12, 0.7, -0.8, 0.05, 0.33, -0.4, 0.9, -0.2];
        let result = classify_output_consistency(&expected, &shuffled);
        assert!(
            matches!(result, OutputConsistency::Diverged { .. }),
            "reordered outputs must be detected, got {result:?}"
        );
    }

    #[test]
    fn output_consistency_wrong_region_detected() {
        let expected = [0.9, -0.4, 0.05, 0.7, -0.2, 0.33, -0.8, 0.12];
        let other_region = [0.01, 0.02, -0.99, 0.5, 0.5, -0.5, 0.44, 0.9];
        let result = classify_output_consistency(&expected, &other_region);
        assert!(
            matches!(result, OutputConsistency::Diverged { .. }),
            "unrelated region must be detected, got {result:?}"
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
    fn is_slice_skipped_false_before_noting() {
        let mgr = make_manager_with_run("run-1");
        assert!(!mgr.is_slice_skipped("run-1", "slice_a"));
    }

    #[test]
    fn is_slice_skipped_true_after_noting() {
        let mut mgr = make_manager_with_run("run-1");
        mgr.note_slice_skipped("run-1", "slice_a");
        assert!(mgr.is_slice_skipped("run-1", "slice_a"));
        assert!(!mgr.is_slice_skipped("run-1", "slice_b"));
    }

    #[test]
    fn is_slice_skipped_scoped_to_run_uid() {
        let mut mgr = make_manager_with_run("run-1");
        mgr.start_run(
            "run-2".to_string(),
            "test-circuit".to_string(),
            "test".to_string(),
            RunSource::Benchmark,
            None,
            None,
        );
        mgr.note_slice_skipped("run-1", "slice_a");
        assert!(mgr.is_slice_skipped("run-1", "slice_a"));
        assert!(!mgr.is_slice_skipped("run-2", "slice_a"));
    }

    #[test]
    fn skipped_slices_cleared_on_run_removal() {
        let mut mgr = make_manager_with_run("run-1");
        mgr.note_slice_skipped("run-1", "slice_a");
        mgr.remove_run("run-1");
        assert!(!mgr.is_slice_skipped("run-1", "slice_a"));
        assert_eq!(mgr.skipped_slice_count("run-1"), 0);
    }

    #[test]
    fn output_consistency_normalization_correction() {
        let onnx_expected = [10.0, 20.0, 30.0];
        let norm_factor = 100.0;
        let zk_outputs: Vec<f64> = onnx_expected.iter().map(|v| v / norm_factor).collect();
        let normalized: Vec<f64> = onnx_expected.iter().map(|v| v / norm_factor).collect();
        let result = classify_output_consistency(&normalized, &zk_outputs);
        assert!(
            matches!(result, OutputConsistency::Consistent { max_rel_err } if max_rel_err == 0.0),
            "normalization-corrected outputs should match exactly, got {result:?}"
        );
    }

    #[test]
    fn output_consistency_bilinear_double_scaling_tolerated() {
        let onnx_expected = [10.0, -20.0, 30.0, -5.0];
        let norm_factor = 3.74_f64;
        let zk_outputs: Vec<f64> = onnx_expected
            .iter()
            .map(|v| v / (norm_factor * norm_factor))
            .collect();
        let normalized: Vec<f64> = onnx_expected.iter().map(|v| v / norm_factor).collect();
        let result = classify_output_consistency(&normalized, &zk_outputs);
        assert!(
            matches!(result, OutputConsistency::Consistent { .. }),
            "bilinear slices scale outputs by the squared factor, got {result:?}"
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
