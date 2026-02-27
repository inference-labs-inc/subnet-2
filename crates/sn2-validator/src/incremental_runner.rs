use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::tensor_json::{arrayd_to_json, json_to_arrayd};
use dsperse::pipeline::{IncrementalRun, SliceExecutionResult, SliceWork};
use dsperse::schema::execution::ExecutionInfo;
use sn2_types::{ProofSystem, RunSource};
use tracing::{info, warn};

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
    pub artifacts: Vec<SliceArtifact>,
    pub relay_request_id: Option<String>,
    pub incremental: Option<IncrementalRun>,
}

pub struct NextSliceInfo {
    pub slice_id: String,
    pub inputs_json: serde_json::Value,
    pub use_circuit: bool,
    pub onnx_path: Option<String>,
    pub input_tensor: ndarray::ArrayD<f64>,
}

#[derive(Default)]
pub struct IncrementalRunManager {
    runs: HashMap<String, ActiveRun>,
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
        self.runs.insert(
            run_uid.clone(),
            ActiveRun {
                run_uid,
                circuit_id,
                circuit_name,
                run_source,
                started_at: Instant::now(),
                artifacts: Vec::new(),
                relay_request_id,
                incremental,
            },
        );
    }

    pub fn has_run(&self, run_uid: &str) -> bool {
        self.runs.contains_key(run_uid)
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
            input_tensor: work.input,
        }))
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
                method: "remote_miner".to_string(),
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

    pub fn take_artifacts(&mut self, run_uid: &str) -> Vec<SliceArtifact> {
        self.runs
            .get_mut(run_uid)
            .map(|r| std::mem::take(&mut r.artifacts))
            .unwrap_or_default()
    }

    pub fn remove_run(&mut self, run_uid: &str) -> Option<ActiveRun> {
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

    pub fn gc_stale(&mut self, max_age: Duration) -> Vec<String> {
        let now = Instant::now();
        let stale: Vec<String> = self
            .runs
            .iter()
            .filter(|(_, run)| now.duration_since(run.started_at) >= max_age)
            .map(|(uid, _)| uid.clone())
            .collect();
        if !stale.is_empty() {
            info!(count = stale.len(), run_uids = ?stale, "evicting stale runs");
        }
        for uid in &stale {
            self.runs.remove(uid);
        }
        stale
    }
}
