use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use rand::Rng;
use sn2_types::*;
use tracing::{info, warn};

use super::ValidatorLoop;
use crate::relay::DsperseSubmission;

enum ExpectedInputs {
    NoMetadata,
    Count(usize),
    Invalid,
}

enum TiledPayload {
    SingleInput(Vec<ndarray::ArrayD<f64>>),
    MultiInput(Vec<Vec<f64>>),
}

struct BundleDispatchMismatch {
    bundle_expected: usize,
    per_request_actual: usize,
    strategy: &'static str,
}

struct PreflightOutcome {
    kept: Vec<dsperse::pipeline::SliceWork>,
    failed: Vec<String>,
    unsatisfiable: usize,
}

impl TiledPayload {
    fn len(&self) -> usize {
        match self {
            Self::SingleInput(v) => v.len(),
            Self::MultiInput(v) => v.len(),
        }
    }
}

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
            Ok(c) => std::sync::Arc::new(c),
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

    /// Compare the per-request payload size that this work item will dispatch
    /// against what the slice's compiled circuit actually consumes per witness.
    /// Returns `Some(mismatch)` only when both can be determined and they
    /// disagree; `None` means the check could not be performed (no bundle, no
    /// onnx, etc.) or the sizes match.
    ///
    /// The bundle's `params.inputs` declare the witness-side shape contract.
    /// `params.weights_as_inputs` slices reserve the trailing entries for
    /// onnx initializers; the activation prefix length is
    /// `params.inputs.len() - initializers.len()`. The sum of shape products
    /// over that prefix is what the witness solver enforces.
    fn bundle_dispatch_mismatch(
        work: &dsperse::pipeline::SliceWork,
    ) -> Option<BundleDispatchMismatch> {
        let circuit_path = work.circuit_path.as_ref()?;
        let backend = dsperse::backend::jstprove::JstproveBackend::new();
        let params = match backend.load_params(std::path::Path::new(circuit_path)) {
            Ok(Some(p)) => p,
            _ => return None,
        };

        let initializer_count = if params.weights_as_inputs {
            let onnx = work.onnx_path.as_ref()?;
            match dsperse::pipeline::extract_onnx_initializers(std::path::Path::new(onnx), &params)
            {
                Ok(v) => v.len(),
                Err(_) => return None,
            }
        } else {
            0
        };

        let activation_entries = params.inputs.len().checked_sub(initializer_count)?;
        let bundle_expected: usize = params.inputs[..activation_entries]
            .iter()
            .map(|io| io.shape.iter().product::<usize>())
            .sum();

        let (per_request_actual, strategy) = if let Some(tiling) = &work.tiling {
            let n = std::cmp::max(work.named_inputs.len(), 1);
            let per_input = if tiling.ndim == 1 {
                tiling.segment_size.unwrap_or(0)
            } else {
                let tiles = if work.named_inputs.len() > 1 {
                    dsperse::pipeline::split_for_multi_input_dispatch(&work.named_inputs, tiling)
                        .ok()
                        .and_then(|v| v.into_iter().next())
                        .map(|flat| flat.len() / n)
                } else {
                    dsperse::pipeline::split_for_tiling(&work.input, tiling)
                        .ok()
                        .and_then(|v| v.into_iter().next())
                        .map(|t| t.len())
                };
                tiles.unwrap_or(0)
            };
            (
                per_input.checked_mul(n).unwrap_or(0),
                if work.named_inputs.len() > 1 {
                    "tiled-multi-input"
                } else {
                    "tiled-single-input"
                },
            )
        } else {
            (work.input.len(), "single")
        };

        // The only safe early-out is "both sides agree on zero". Treating
        // either side's zero independently as a free pass would let an
        // asymmetric configuration (compiled for empty activations but
        // dispatched with N elements, or vice versa) slip through preflight.
        if per_request_actual == bundle_expected {
            None
        } else {
            Some(BundleDispatchMismatch {
                bundle_expected,
                per_request_actual,
                strategy,
            })
        }
    }

    /// Synchronous preflight loop, intended to be called from inside a
    /// `tokio::task::spawn_blocking` scope. Each work item is validated
    /// against (a) its model-level aggregate input shape and (b) its
    /// compiled circuit's per-witness expected payload size; mismatches
    /// produce a slice-id entry in `failed` and are dropped from `kept`.
    fn preflight_work_items(
        run_uid: &str,
        work_items: Vec<dsperse::pipeline::SliceWork>,
    ) -> PreflightOutcome {
        let mut kept = Vec::with_capacity(work_items.len());
        let mut failed: Vec<String> = Vec::new();
        let mut unsatisfiable = 0usize;
        for work in work_items {
            match Self::expected_input_elements(&work.slice_meta.input_shape) {
                ExpectedInputs::Count(expected) if expected != work.input.len() => {
                    warn!(
                        run_uid = %run_uid,
                        slice = %work.slice_id,
                        expected,
                        actual = work.input.len(),
                        "preflight: slice input activation count does not match circuit expectation, skipping"
                    );
                    failed.push(work.slice_id.clone());
                    unsatisfiable += 1;
                    continue;
                }
                ExpectedInputs::Invalid => {
                    warn!(
                        run_uid = %run_uid,
                        slice = %work.slice_id,
                        input_shape = ?work.slice_meta.input_shape,
                        "preflight: slice input shape metadata is invalid (non-positive, overflow, or out-of-range), skipping"
                    );
                    failed.push(work.slice_id.clone());
                    unsatisfiable += 1;
                    continue;
                }
                ExpectedInputs::Count(_) | ExpectedInputs::NoMetadata => {}
            }

            if let Some(mismatch) = Self::bundle_dispatch_mismatch(&work) {
                warn!(
                    run_uid = %run_uid,
                    slice = %work.slice_id,
                    bundle_expected = mismatch.bundle_expected,
                    per_request_actual = mismatch.per_request_actual,
                    strategy = mismatch.strategy,
                    "preflight: per-request payload size will not match the slice's compiled circuit, skipping"
                );
                failed.push(work.slice_id.clone());
                unsatisfiable += 1;
                continue;
            }

            kept.push(work);
        }
        PreflightOutcome {
            kept,
            failed,
            unsatisfiable,
        }
    }

    fn expected_input_elements(input_shape: &[Vec<i64>]) -> ExpectedInputs {
        if input_shape.is_empty() {
            return ExpectedInputs::NoMetadata;
        }
        let mut total: usize = 0;
        for shape in input_shape {
            let mut product: usize = 1;
            for &dim in shape {
                if dim <= 0 {
                    return ExpectedInputs::Invalid;
                }
                let Ok(dim_usize) = usize::try_from(dim) else {
                    return ExpectedInputs::Invalid;
                };
                let Some(next) = product.checked_mul(dim_usize) else {
                    return ExpectedInputs::Invalid;
                };
                product = next;
            }
            let Some(next_total) = total.checked_add(product) else {
                return ExpectedInputs::Invalid;
            };
            total = next_total;
        }
        ExpectedInputs::Count(total)
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
        circuit: &Arc<Circuit>,
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

        // Bundle preflight reads each slice's manifest.msgpack and (for WAI
        // bundles) the slice's onnx graph; those are synchronous disk reads
        // that must not run on the async reactor. Move the entire preflight
        // loop into a blocking task and rejoin afterwards to drive
        // run_manager updates from the async context.
        let preflight_run_uid = run_uid.to_string();
        let preflight = tokio::task::spawn_blocking(move || {
            Self::preflight_work_items(&preflight_run_uid, work_items)
        })
        .await;
        let PreflightOutcome {
            kept: work_items,
            failed: preflight_failed,
            unsatisfiable,
        } = match preflight {
            Ok(outcome) => outcome,
            Err(e) => {
                warn!(run_uid = %run_uid, error = %e, "preflight task panicked");
                self.teardown_run(run_uid).await;
                return;
            }
        };
        for slice_id in &preflight_failed {
            self.run_manager.mark_slice_failed(run_uid, slice_id);
        }
        if unsatisfiable > 0 {
            info!(
                run_uid = %run_uid,
                circuit_id = %circuit.id,
                unsatisfiable,
                "preflight filtered slices due to mismatched activation sizes or invalid metadata"
            );
        }

        if work_items.is_empty() {
            info!(run_uid = %run_uid, "no circuit slices to dispatch, completing run");
            self.finalize_combined_run(run_uid).await;
            return;
        }

        let mut staged = StagedWork::new();

        for work in work_items {
            let mut input_tensor = work.input;
            let named_inputs = work.named_inputs;

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
                    &named_inputs,
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
                let inputs_bytes = crate::tensor::input_data_payload(&input_tensor);
                staged.stage_request(DSliceRequest {
                    circuit: Arc::clone(circuit),
                    inputs: inputs_bytes,
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
        circuit: &Arc<Circuit>,
        slice_id: &str,
        circuit_path: Option<&str>,
        component_sha: Option<&str>,
        tiling: &dsperse::schema::tiling::TilingInfo,
        input_tensor: ndarray::ArrayD<f64>,
        named_inputs: &[(String, ndarray::ArrayD<f64>)],
        run_source: RunSource,
        prove_pct: f64,
    ) -> Option<usize> {
        let multi_input = named_inputs.len() > 1;
        let tiles_payload = if multi_input {
            match dsperse::pipeline::split_for_multi_input_dispatch(named_inputs, tiling) {
                Ok(per_tile) => TiledPayload::MultiInput(per_tile),
                Err(e) => {
                    warn!(
                        run_uid = %run_uid,
                        slice = %slice_id,
                        error = %e,
                        "split_for_multi_input_dispatch failed"
                    );
                    return None;
                }
            }
        } else {
            match dsperse::pipeline::split_for_tiling(&input_tensor, tiling) {
                Ok(t) => TiledPayload::SingleInput(t),
                Err(e) => {
                    warn!(run_uid = %run_uid, slice = %slice_id, error = %e, "split_for_tiling failed");
                    return None;
                }
            }
        };

        let num_tiles = tiles_payload.len();
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

        let mut prepared: Vec<(usize, bytes::Bytes)> = Vec::with_capacity(sampled_indices.len());
        match tiles_payload {
            TiledPayload::SingleInput(tiles) => {
                for (idx, tile) in tiles.into_iter().enumerate() {
                    if !sampled_indices.contains(&idx) {
                        continue;
                    }
                    let tile_bytes = crate::tensor::input_data_payload(&tile.into_dyn());
                    prepared.push((idx, tile_bytes));
                }
            }
            TiledPayload::MultiInput(per_tile) => {
                for (idx, flat) in per_tile.into_iter().enumerate() {
                    if !sampled_indices.contains(&idx) {
                        continue;
                    }
                    let len = flat.len();
                    let tile_arr =
                        match ndarray::ArrayD::from_shape_vec(ndarray::IxDyn(&[len]), flat) {
                            Ok(arr) => arr,
                            Err(e) => {
                                warn!(
                                    run_uid = %run_uid,
                                    slice = %slice_id,
                                    tile_idx = idx,
                                    error = %e,
                                    "skipping multi-input tile: from_shape_vec rejected 1-D shape"
                                );
                                continue;
                            }
                        };
                    let tile_bytes = crate::tensor::input_data_payload(&tile_arr);
                    prepared.push((idx, tile_bytes));
                }
            }
        }

        if prepared.is_empty() {
            warn!(
                run_uid = %run_uid,
                slice = %slice_id,
                "no tiles survived staging, aborting slice"
            );
            return None;
        }

        let expected_indices: std::collections::HashSet<u32> =
            prepared.iter().map(|(idx, _)| *idx as u32).collect();
        if let Err(e) = run_manager.init_tile_counter(run_uid, slice_id, tiling, expected_indices) {
            warn!(run_uid = %run_uid, slice = %slice_id, error = %e, "init_tile_counter failed");
            return None;
        }

        let staged_count = prepared.len();
        for (idx, tile_bytes) in prepared {
            staged.stage_request(Self::build_tile_request(
                circuit,
                slice_id,
                run_uid,
                tile_bytes,
                idx as u32,
                run_source,
                circuit_path,
                component_sha,
            ));
        }

        Some(staged_count)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_tile_request(
        circuit: &Arc<Circuit>,
        slice_id: &str,
        run_uid: &str,
        tile_bytes: bytes::Bytes,
        tile_idx: u32,
        run_source: RunSource,
        circuit_path: Option<&str>,
        component_sha: Option<&str>,
    ) -> DSliceRequest {
        DSliceRequest {
            circuit: Arc::clone(circuit),
            inputs: tile_bytes,
            request_type: RequestType::DSlice,
            proof_system: circuit.proof_system,
            slice_num: slice_id.to_string(),
            run_uid: run_uid.to_string(),
            outputs: None,
            is_tile: true,
            tile_idx: Some(tile_idx),
            task_id: None,
            run_source,
            retry_count: 0,
            circuit_path: circuit_path.map(String::from),
            component_sha: component_sha.map(String::from),
        }
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
                let flat = match dims.first().and_then(|d| d.as_array()) {
                    Some(nested) => nested,
                    None => dims,
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

        let circuit = Arc::new(circuit.clone());
        self.enqueue_all_dslices(&run_uid, &circuit, RunSource::Benchmark, 1.0)
            .await;
    }
}
