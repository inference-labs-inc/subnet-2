use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::tensor_json::{arrayd_to_json, json_to_arrayd};
use dsperse::pipeline::{IncrementalRun, SliceExecutionResult, SliceWork};
use dsperse::schema::execution::{ExecutionInfo, ExecutionMethod};
use dsperse::schema::tiling::TilingInfo;
use sn2_types::{ProofSystem, RunSource};
use tracing::{info, warn};

pub enum TileBufferOutcome {
    Waiting,
    Ready(ndarray::ArrayD<f64>),
    Failed(String),
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
    pub relay_request_id: Option<String>,
    pub incremental: Option<IncrementalRun>,
}

pub struct NextSliceInfo {
    pub slice_id: String,
    pub inputs_json: serde_json::Value,
    pub use_circuit: bool,
    pub onnx_path: Option<String>,
    pub circuit_path: Option<String>,
    pub input_tensor: ndarray::ArrayD<f64>,
    pub named_inputs: Vec<(String, ndarray::ArrayD<f64>)>,
    pub tiling: Option<TilingInfo>,
}

struct TileBuffer {
    tiling: TilingInfo,
    tiles: Vec<Option<ndarray::ArrayD<f64>>>,
}

const EVICTED_CAP: usize = 256;

#[derive(Default)]
pub struct IncrementalRunManager {
    runs: HashMap<String, ActiveRun>,
    evicted_set: HashSet<String>,
    evicted_order: VecDeque<String>,
    tile_buffers: HashMap<(String, String), TileBuffer>,
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
        relay_request_id: Option<String>,
        incremental: Option<IncrementalRun>,
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
                incremental,
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
        let inc = match run.incremental.as_ref() {
            Some(i) => i,
            None => return (0, 0, HashMap::new()),
        };
        let meta = inc.model_meta();
        let total_slices = meta.slices.len();
        let mut map = HashMap::with_capacity(total_slices);
        let mut total_tiles = 0usize;
        for s in &meta.slices {
            let tiles = s.tiling.as_ref().map(|t| t.num_tiles).unwrap_or(1);
            map.insert(format!("slice_{}", s.index), tiles);
            total_tiles += tiles;
        }
        (total_slices, total_tiles, map)
    }

    pub fn is_evicted(&self, run_uid: &str) -> bool {
        self.evicted_set.contains(run_uid)
    }

    fn mark_evicted(&mut self, run_uid: String) {
        if self.evicted_set.insert(run_uid.clone()) {
            self.evicted_order.push_back(run_uid);
        }
        while self.evicted_order.len() > EVICTED_CAP {
            if let Some(oldest) = self.evicted_order.pop_front() {
                self.evicted_set.remove(&oldest);
            }
        }
    }

    pub fn get_circuit_id(&self, run_uid: &str) -> Option<&str> {
        self.runs.get(run_uid).map(|r| r.circuit_id.as_str())
    }

    pub fn get_run_source(&self, run_uid: &str) -> Option<RunSource> {
        self.runs.get(run_uid).map(|r| r.run_source)
    }

    pub fn next_slice(&self, run_uid: &str) -> anyhow::Result<Option<NextSliceInfo>> {
        let run = match self.runs.get(run_uid) {
            Some(r) => r,
            None => return Ok(None),
        };
        let inc = match run.incremental.as_ref() {
            Some(i) => i,
            None => return Ok(None),
        };
        let work: SliceWork = match inc.next_slice()? {
            Some(w) => w,
            None => return Ok(None),
        };
        let inputs_json = serde_json::json!({
            "input_data": arrayd_to_json(&work.input)
        });
        Ok(Some(NextSliceInfo {
            slice_id: work.slice_id,
            inputs_json,
            use_circuit: work.use_circuit,
            onnx_path: work.onnx_path,
            circuit_path: work.circuit_path,
            input_tensor: work.input,
            named_inputs: work.named_inputs,
            tiling: work.tiling,
        }))
    }

    pub fn init_tile_buffer(
        &mut self,
        run_uid: &str,
        slice_id: &str,
        tiling: TilingInfo,
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
            tiles_y = tiling.tiles_y,
            tiles_x = tiling.tiles_x,
            "initialized tile buffer"
        );
        let num_tiles = tiling.num_tiles;
        let key = (run_uid.to_string(), slice_id.to_string());
        use std::collections::hash_map::Entry;
        match self.tile_buffers.entry(key) {
            Entry::Vacant(e) => {
                e.insert(TileBuffer {
                    tiling,
                    tiles: vec![None; num_tiles],
                });
            }
            Entry::Occupied(_) => {
                return Err(anyhow::anyhow!(
                    "tile buffer already exists for run {run_uid}, slice {slice_id}"
                ));
            }
        }
        Ok(())
    }

    pub fn buffer_tile_result(
        &mut self,
        run_uid: &str,
        slice_id: &str,
        tile_idx: u32,
        output: ndarray::ArrayD<f64>,
    ) -> TileBufferOutcome {
        if let Some(run) = self.runs.get_mut(run_uid) {
            run.last_activity = Instant::now();
        }
        let key = (run_uid.to_string(), slice_id.to_string());
        let buf = match self.tile_buffers.get_mut(&key) {
            Some(b) => b,
            None => {
                return TileBufferOutcome::Failed(format!(
                    "no tile buffer for run={run_uid} slice={slice_id}"
                ));
            }
        };
        let idx = tile_idx as usize;
        if idx >= buf.tiles.len() {
            return TileBufferOutcome::Failed(format!(
                "tile_idx {tile_idx} out of range (expected < {})",
                buf.tiles.len()
            ));
        }
        buf.tiles[idx] = Some(output);

        let received = buf.tiles.iter().filter(|t| t.is_some()).count();
        let total = buf.tiles.len();
        info!(
            run_uid = %run_uid,
            slice = %slice_id,
            tile_idx = tile_idx,
            received = received,
            total = total,
            "buffered tile result"
        );

        if received < total {
            return TileBufferOutcome::Waiting;
        }

        let buf = self.tile_buffers.get(&key).unwrap();
        let tile_outputs: Vec<ndarray::ArrayD<f64>> =
            buf.tiles.iter().map(|t| t.clone().unwrap()).collect();

        match dsperse::pipeline::reconstruct_from_tiles(&tile_outputs, &buf.tiling) {
            Ok(full) => {
                self.tile_buffers.remove(&key);
                info!(
                    run_uid = %run_uid,
                    slice = %slice_id,
                    output_shape = ?full.shape(),
                    "reconstructed full output from tiles"
                );
                TileBufferOutcome::Ready(full)
            }
            Err(e) => TileBufferOutcome::Failed(format!(
                "tile reconstruction failed for run={run_uid} slice={slice_id}: {e}"
            )),
        }
    }

    pub fn apply_result(
        &mut self,
        run_uid: &str,
        slice_id: &str,
        computed_outputs: &serde_json::Value,
    ) -> anyhow::Result<bool> {
        let run = self
            .runs
            .get_mut(run_uid)
            .ok_or_else(|| anyhow::anyhow!("unknown run {run_uid}"))?;
        run.last_activity = Instant::now();
        let inc = run
            .incremental
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("run {run_uid} has no IncrementalRun"))?;

        let output_tensor = json_to_arrayd(computed_outputs)
            .map_err(|e| anyhow::anyhow!("output tensor conversion: {e}"))?;

        inc.apply_result(SliceExecutionResult {
            slice_id: slice_id.to_string(),
            output: output_tensor,
            execution_info: ExecutionInfo {
                method: ExecutionMethod::OnnxOnly,
                success: true,
                error: None,
                witness_file: None,
                tile_exec_infos: Vec::new(),
            },
        })
        .map_err(|e| anyhow::anyhow!("apply_result: {e}"))?;

        Ok(inc.is_complete())
    }

    pub fn final_output_json(&self, run_uid: &str) -> Option<serde_json::Value> {
        let run = self.runs.get(run_uid)?;
        let inc = run.incremental.as_ref()?;
        let output = inc.final_output()?;
        Some(arrayd_to_json(output))
    }

    pub fn push_artifact(&mut self, run_uid: &str, artifact: SliceArtifact) {
        if let Some(run) = self.runs.get_mut(run_uid) {
            run.artifacts.push(artifact);
        }
    }

    pub fn remove_run(&mut self, run_uid: &str) -> Option<ActiveRun> {
        self.tile_buffers.retain(|(uid, _), _| uid != run_uid);
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

    pub fn evict_by_circuit(&mut self, circuit_id: &str) -> Vec<String> {
        let to_remove: Vec<String> = self
            .runs
            .iter()
            .filter(|(_, run)| run.circuit_id == circuit_id)
            .map(|(uid, _)| uid.clone())
            .collect();
        let evict_set: HashSet<&str> = to_remove.iter().map(|s| s.as_str()).collect();
        self.tile_buffers
            .retain(|(run_uid, _), _| !evict_set.contains(run_uid.as_str()));
        for uid in to_remove.iter() {
            self.runs.remove(uid);
            self.mark_evicted(uid.clone());
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
        self.tile_buffers
            .retain(|(run_uid, _), _| !stale_set.contains(run_uid.as_str()));
        for uid in stale.iter() {
            self.runs.remove(uid);
            self.mark_evicted(uid.clone());
        }
        stale
    }
}
