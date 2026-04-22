use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::Result;
use rand::Rng;
use sn2_types::*;
use tracing::{info, warn};

use super::ValidatorLoop;
use crate::relay::DsperseSubmission;

struct StagedWork {
    requests: VecDeque<DSliceRequest>,
    events: Vec<(String, String, usize)>,
}

impl StagedWork {
    fn new() -> Self {
        Self {
            requests: VecDeque::new(),
            events: Vec::new(),
        }
    }

    fn stage_request(&mut self, request: DSliceRequest) {
        self.requests.push_back(request);
    }

    fn total_queued(&self) -> usize {
        self.requests.len()
    }
}

impl ValidatorLoop {
    pub(super) fn decode_submission_tensor(
        submission: &DsperseSubmission,
        circuit: &Circuit,
    ) -> Result<ndarray::ArrayD<f64>> {
        if let Some(tensor_bytes) = &submission.tensor_data {
            let shape: Vec<usize> = circuit
                .metadata
                .input_schema
                .as_ref()
                .and_then(|s| s.get("shape"))
                .and_then(|v| v.as_array())
                .and_then(|dims| {
                    dims.iter()
                        .map(|d| d.as_u64().and_then(|n| usize::try_from(n).ok()))
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or_else(|| anyhow::anyhow!("circuit schema missing shape"))?;
            crate::tensor::decode_gzipped_protobuf_tensor(tensor_bytes, &shape)
        } else {
            let tensor_value = submission
                .inputs
                .get("input_data")
                .unwrap_or(&submission.inputs);
            crate::tensor::json_to_arrayd(tensor_value)
        }
    }

    pub(super) async fn handle_dsperse_submission(&mut self, submission: DsperseSubmission) {
        let circuit = match self
            .circuit_store
            .ensure_circuit(&submission.circuit_id)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                warn!(circuit = %submission.circuit_id, error = %e, "unknown circuit in dsperse submission");
                self.send_submit_error(submission.request_id, &format!("unknown circuit: {e}"))
                    .await;
                return;
            }
        };

        if submission.tensor_data.is_none() {
            if let Err(msg) = circuit.validate_inputs(&submission.inputs) {
                warn!(circuit = %circuit.id, error = %msg, "invalid inputs for dsperse submission");
                self.send_submit_error(
                    submission.request_id,
                    &format!("invalid input shape: {msg}"),
                )
                .await;
                return;
            }
        }

        let slices_dir = circuit.paths.base_path.join("slices");
        let input_tensor = match Self::decode_submission_tensor(&submission, &circuit) {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, "failed to decode submission tensor");
                self.send_submit_error(submission.request_id, "invalid tensor payload")
                    .await;
                return;
            }
        };

        let dir = slices_dir.clone();
        let combined = match tokio::task::spawn_blocking(move || {
            dsperse::pipeline::CombinedRun::new(&dir, input_tensor)
        })
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                warn!(error = %e, circuit = %circuit.id, "combined inference failed");
                self.send_submit_error(submission.request_id, "combined inference failed")
                    .await;
                return;
            }
            Err(e) => {
                warn!(error = %e, circuit = %circuit.id, "combined inference task panicked");
                self.send_submit_error(submission.request_id, "combined inference panicked")
                    .await;
                return;
            }
        };

        let run_uid = uuid::Uuid::new_v4().to_string();
        info!(run_uid = %run_uid, circuit = %circuit.id, "started combined run");

        self.run_manager.start_run(
            run_uid.clone(),
            circuit.id.clone(),
            circuit.metadata.name.clone(),
            RunSource::Api,
            submission.request_id,
            Some(combined),
        );

        let (total_slices, total_tiles, stc) = self.run_manager.slice_tile_counts(&run_uid);

        {
            let uid = run_uid.clone();
            let cid = circuit.id.clone();
            let cname = circuit.metadata.name.clone();
            self.emit_event(move |ev| async move {
                ev.emit_run_started(&uid, &cid, &cname, total_slices, total_tiles, &stc, "api")
                    .await;
            });
        }

        self.relay_register_pending(&run_uid).await;

        if let Some(req_id) = submission.request_id {
            use crate::relay::FRAME_SUBMIT_RESULT;
            self.relay_send_response(
                FRAME_SUBMIT_RESULT,
                req_id,
                serde_json::json!({
                    "run_uid": run_uid,
                    "status": "started",
                    "total_slices": total_slices,
                    "total_tiles": total_tiles,
                }),
            )
            .await;
        }

        let benchmark_uids = self.run_manager.benchmark_run_uids();
        for uid in &benchmark_uids {
            info!(run_uid = %uid, "preempting benchmark run for API dsperse submission");
            self.teardown_run(uid).await;
        }
        self.stacked_dslice_queue.clear();
        self.dsperse_benchmark_backoff_until = Instant::now() + Duration::from_secs(120);

        self.enqueue_all_dslices(&run_uid, &circuit, RunSource::Api, submission.prove_pct)
            .await;
    }

    pub(super) async fn cleanup_run_resources(&mut self, _run_uid: &str) {
        // Composable model components persist on disk across runs — no cleanup needed.
    }

    fn normalize_tensor(
        &mut self,
        run_uid: &str,
        slice_id: &str,
        input_tensor: &mut ndarray::ArrayD<f64>,
    ) {
        let input_max_abs = input_tensor.iter().fold(0.0_f64, |m, v| m.max(v.abs()));
        if input_max_abs > 1.0 {
            input_tensor.mapv_inplace(|v| v / input_max_abs);
            self.dslice_input_scales
                .insert((run_uid.to_string(), slice_id.to_string()), input_max_abs);
            info!(
                run_uid = %run_uid,
                slice = %slice_id,
                input_max_abs,
                "normalized circuit slice inputs to [-1, 1]"
            );
        }
    }

    fn flush_staged(&mut self, staged: StagedWork) {
        for request in staged.requests {
            match request.run_source {
                RunSource::Api => self.api_dslice_queue.push_back(request),
                RunSource::Benchmark => self.stacked_dslice_queue.push_back(request),
            }
        }

        for (uid, snum, count) in staged.events {
            self.emit_event(move |ev| async move {
                ev.emit_work_items_created(&uid, &snum, count).await;
            });
        }

        self.dispatch_notify.notify_one();
    }

    pub(super) async fn enqueue_all_dslices(
        &mut self,
        run_uid: &str,
        circuit: &Circuit,
        run_source: RunSource,
        prove_pct: f64,
    ) {
        let clamped_prove_pct = if !prove_pct.is_finite() || prove_pct <= 0.0 || prove_pct > 1.0 {
            1.0
        } else {
            prove_pct
        };
        let work_items = match self.run_manager.all_circuit_work(run_uid) {
            Ok(items) => items,
            Err(e) => {
                warn!(run_uid = %run_uid, error = %e, "failed to enumerate circuit work");
                self.teardown_run(run_uid).await;
                return;
            }
        };

        let work_items: Vec<_> = {
            let disabled = self.disabled_slices.get(&circuit.id).cloned();
            match disabled {
                Some(disabled) if !disabled.is_empty() => {
                    let (kept, skipped): (Vec<_>, Vec<_>) = work_items
                        .into_iter()
                        .partition(|w| !disabled.contains(&w.slice_id));
                    for work in &skipped {
                        self.run_manager.mark_slice_failed(run_uid, &work.slice_id);
                    }
                    if !skipped.is_empty() {
                        info!(
                            run_uid = %run_uid,
                            circuit_id = %circuit.id,
                            skipped = skipped.len(),
                            "skipping slices previously disabled for this circuit"
                        );
                    }
                    kept
                }
                _ => work_items,
            }
        };

        if work_items.is_empty() {
            info!(run_uid = %run_uid, "no circuit slices to dispatch, completing run");
            self.finalize_combined_run(run_uid).await;
            return;
        }

        let mut staged = StagedWork::new();

        for work in work_items {
            let mut input_tensor = work.input;

            if input_tensor.iter().any(|v| !v.is_finite()) {
                warn!(
                    run_uid = %run_uid,
                    slice = %work.slice_id,
                    "circuit slice input contains non-finite values, aborting run"
                );
                self.teardown_run(run_uid).await;
                return;
            }

            self.normalize_tensor(run_uid, &work.slice_id, &mut input_tensor);

            let comp_sha = self
                .circuit_store
                .component_sha(&circuit.id, &work.slice_id);

            let queued = if let Some(ref tiling) = work.tiling {
                match Self::stage_tiled_work(
                    &mut staged,
                    &mut self.run_manager,
                    run_uid,
                    circuit,
                    &work.slice_id,
                    work.circuit_path.as_deref(),
                    comp_sha,
                    tiling,
                    input_tensor,
                    run_source,
                    clamped_prove_pct,
                ) {
                    Some(n) => n,
                    None => {
                        self.teardown_run(run_uid).await;
                        return;
                    }
                }
            } else {
                let inputs_json = serde_json::json!({
                    "input_data": crate::tensor::arrayd_to_json(&input_tensor)
                });
                staged.stage_request(DSliceRequest {
                    circuit: circuit.clone(),
                    inputs: inputs_json,
                    request_type: RequestType::DSlice,
                    proof_system: circuit.proof_system,
                    slice_num: work.slice_id.clone(),
                    run_uid: run_uid.to_string(),
                    outputs: None,
                    is_tile: false,
                    tile_idx: None,
                    task_id: None,
                    run_source,
                    retry_count: 0,
                    circuit_path: work.circuit_path.clone(),
                    component_sha: comp_sha.map(String::from),
                });
                1
            };

            staged
                .events
                .push((run_uid.to_string(), work.slice_id.clone(), queued));
        }

        let total_queued = staged.total_queued();
        self.flush_staged(staged);

        info!(
            run_uid = %run_uid,
            total_queued,
            "queued all circuit work items for combined run"
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn stage_tiled_work(
        staged: &mut StagedWork,
        run_manager: &mut crate::incremental_runner::IncrementalRunManager,
        run_uid: &str,
        circuit: &Circuit,
        slice_id: &str,
        circuit_path: Option<&str>,
        component_sha: Option<&str>,
        tiling: &dsperse::schema::tiling::TilingInfo,
        input_tensor: ndarray::ArrayD<f64>,
        run_source: RunSource,
        prove_pct: f64,
    ) -> Option<usize> {
        let tiles = match dsperse::pipeline::split_for_tiling(&input_tensor, tiling) {
            Ok(t) => t,
            Err(e) => {
                warn!(run_uid = %run_uid, slice = %slice_id, error = %e, "split_for_tiling failed");
                return None;
            }
        };

        let num_tiles = tiles.len();
        if num_tiles == 0 || num_tiles != tiling.num_tiles {
            warn!(
                run_uid = %run_uid,
                slice = %slice_id,
                actual = num_tiles,
                expected = tiling.num_tiles,
                "tile count mismatch or zero"
            );
            return None;
        }

        let sampled_indices =
            Self::sample_tile_indices(num_tiles, prove_pct, run_source, run_uid, slice_id);
        if sampled_indices.is_empty() {
            warn!(
                run_uid = %run_uid,
                slice = %slice_id,
                "prove_pct sampled zero tiles, aborting slice stage"
            );
            return None;
        }

        let mut expected_indices: std::collections::HashSet<u32> =
            std::collections::HashSet::with_capacity(sampled_indices.len());
        for &idx in &sampled_indices {
            expected_indices.insert(idx as u32);
        }

        if let Err(e) = run_manager.init_tile_counter(run_uid, slice_id, tiling, expected_indices) {
            warn!(run_uid = %run_uid, slice = %slice_id, error = %e, "init_tile_counter failed");
            return None;
        }

        for (idx, tile) in tiles.into_iter().enumerate() {
            if !sampled_indices.contains(&idx) {
                continue;
            }
            let tile_json = serde_json::json!({
                "input_data": crate::tensor::arrayd_to_json(&tile.into_dyn())
            });
            staged.stage_request(DSliceRequest {
                circuit: circuit.clone(),
                inputs: tile_json,
                request_type: RequestType::DSlice,
                proof_system: circuit.proof_system,
                slice_num: slice_id.to_string(),
                run_uid: run_uid.to_string(),
                outputs: None,
                is_tile: true,
                tile_idx: Some(idx as u32),
                task_id: None,
                run_source,
                retry_count: 0,
                circuit_path: circuit_path.map(String::from),
                component_sha: component_sha.map(String::from),
            });
        }

        Some(sampled_indices.len())
    }

    fn sample_tile_indices(
        num_tiles: usize,
        prove_pct: f64,
        run_source: RunSource,
        run_uid: &str,
        slice_id: &str,
    ) -> std::collections::HashSet<usize> {
        if run_source == RunSource::Benchmark || prove_pct >= 1.0 || num_tiles <= 1 {
            return (0..num_tiles).collect();
        }
        let target = ((num_tiles as f64) * prove_pct).ceil() as usize;
        let target = target.clamp(1, num_tiles);
        if target >= num_tiles {
            return (0..num_tiles).collect();
        }
        let mut indices: Vec<usize> = (0..num_tiles).collect();
        let mut rng = rand::rng();
        for i in 0..target {
            let j = rng.random_range(i..num_tiles);
            indices.swap(i, j);
        }
        indices.truncate(target);
        info!(
            run_uid = %run_uid,
            slice = %slice_id,
            num_tiles,
            sampled = target,
            prove_pct,
            "sampled subset of tiles for partial proof run"
        );
        indices.into_iter().collect()
    }

    pub(super) async fn finalize_combined_run(&mut self, run_uid: &str) {
        self.cleanup_run_resources(run_uid).await;
        self.dslice_input_scales
            .retain(|(uid, _), _| uid != run_uid);

        let failed_count = self.run_manager.failed_slice_count(run_uid);
        info!(run_uid = %run_uid, failed_count, "combined run complete");

        if let Some(circuit_id) = self
            .run_manager
            .circuit_id_for_run(run_uid)
            .map(str::to_string)
        {
            let (_, _, slice_tiles) = self.run_manager.slice_tile_counts(run_uid);
            let candidates: Vec<String> = slice_tiles
                .into_keys()
                .filter(|slice_id| {
                    self.run_manager.is_slice_failed(run_uid, slice_id)
                        && self.run_manager.verified_tile_count(run_uid, slice_id) == 0
                })
                .collect();
            if !candidates.is_empty() {
                let entry = self.disabled_slices.entry(circuit_id.clone()).or_default();
                let mut inserted = 0usize;
                for slice_id in &candidates {
                    if entry.insert(slice_id.clone()) {
                        inserted += 1;
                    }
                }
                if inserted > 0 {
                    info!(
                        circuit_id = %circuit_id,
                        newly_disabled = inserted,
                        total_disabled_for_circuit = entry.len(),
                        "disabled slices with zero verified tiles"
                    );
                }
            }
        }

        let final_output = self.run_manager.final_output_json(run_uid);
        let mut active_run = self.run_manager.remove_run(run_uid);

        if let Some(ref run) = active_run {
            self.report_dsperse_completion(run);
            self.spawn_emit_run_complete(run, true);
        }

        let relay_output = final_output.clone();
        self.spawn_artifact_upload(run_uid, &mut active_run, final_output);
        self.notify_run_completed(run_uid, &active_run, relay_output, failed_count)
            .await;
    }

    pub(super) async fn replenish_dslice_queues(&mut self) {
        if self.config.disable_benchmark || self.run_manager.has_benchmark_runs() {
            return;
        }
        if !self.api_dslice_queue.is_empty() {
            return;
        }
        if Instant::now() < self.dsperse_benchmark_backoff_until {
            return;
        }
        let dsperse_circuits: Vec<_> = self
            .circuit_store
            .get_dsperse_circuits()
            .into_iter()
            .filter(|c| self.circuit_store.is_dsperse_ready(&c.id))
            .collect();
        if dsperse_circuits.is_empty() {
            return;
        }
        let idx = rand::rng().random_range(0..dsperse_circuits.len());
        let circuit = &dsperse_circuits[idx];

        let schema = match &circuit.metadata.input_schema {
            Some(s) if !s.is_empty() => s.clone(),
            _ => {
                warn!(circuit = %circuit.id, "dsperse circuit has no input_schema, cannot benchmark");
                return;
            }
        };

        let shape: Vec<usize> = match schema.get("shape").and_then(|v| v.as_array()) {
            Some(dims) => {
                let flat = if dims.first().and_then(|d| d.as_array()).is_some() {
                    dims.first().and_then(|d| d.as_array()).unwrap()
                } else {
                    dims
                };
                match flat
                    .iter()
                    .map(|d| d.as_u64().map(|v| v as usize))
                    .collect::<Option<Vec<_>>>()
                    .filter(|s| !s.is_empty() && s.iter().all(|&d| d > 0))
                {
                    Some(s) => s,
                    None => {
                        warn!(circuit = %circuit.id, "cannot derive tensor shape from input_schema");
                        return;
                    }
                }
            }
            None => {
                warn!(circuit = %circuit.id, "cannot derive tensor shape from input_schema");
                return;
            }
        };

        let mut rng = rand::rng();
        let input = ndarray::ArrayD::from_shape_fn(ndarray::IxDyn(&shape), |_| {
            rng.random_range(0.0_f64..1.0)
        });
        let slices_dir = circuit.paths.base_path.join("slices");

        let dir = slices_dir.clone();
        let combined = match tokio::task::spawn_blocking(move || {
            dsperse::pipeline::CombinedRun::new(&dir, input)
        })
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                warn!(error = %e, circuit = %circuit.id, "benchmark combined inference failed");
                self.dsperse_benchmark_backoff_until = Instant::now() + Duration::from_secs(60);
                return;
            }
            Err(e) => {
                warn!(error = %e, circuit = %circuit.id, "benchmark combined inference panicked");
                self.dsperse_benchmark_backoff_until = Instant::now() + Duration::from_secs(60);
                return;
            }
        };

        let run_uid = uuid::Uuid::new_v4().to_string();
        info!(run_uid = %run_uid, circuit = %circuit.id, name = %circuit.metadata.name, "started combined benchmark run");

        self.run_manager.start_run(
            run_uid.clone(),
            circuit.id.clone(),
            circuit.metadata.name.clone(),
            RunSource::Benchmark,
            None,
            Some(combined),
        );

        {
            let (total_slices, total_tiles, stc) = self.run_manager.slice_tile_counts(&run_uid);
            let uid = run_uid.clone();
            let cid = circuit.id.clone();
            let cname = circuit.metadata.name.clone();
            self.emit_event(move |ev| async move {
                ev.emit_run_started(
                    &uid,
                    &cid,
                    &cname,
                    total_slices,
                    total_tiles,
                    &stc,
                    "benchmark",
                )
                .await;
            });
        }

        let circuit = circuit.clone();
        self.enqueue_all_dslices(&run_uid, &circuit, RunSource::Benchmark, 1.0)
            .await;
    }
}
