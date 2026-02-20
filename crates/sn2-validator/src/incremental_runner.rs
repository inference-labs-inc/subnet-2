use std::collections::HashMap;
use std::time::{Duration, Instant};

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
            },
        );
    }

    pub fn has_run(&self, run_uid: &str) -> bool {
        self.runs.contains_key(run_uid)
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
