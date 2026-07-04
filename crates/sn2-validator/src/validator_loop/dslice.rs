use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use rand::Rng;
use sn2_types::*;
use tracing::{info, warn};

use super::ValidatorLoop;
use crate::relay::DsperseSubmission;
use sn2_types::REHAB_BLOCKS as DISABLED_SLICE_REHAB_BLOCKS;

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

pub(crate) enum PlannedPayload {
    Single,
    Tiled {
        tiling: dsperse::schema::tiling::TilingInfo,
        indices: Vec<u32>,
    },
    DimSplit {
        ds: dsperse::schema::tiling::DimSplitInfo,
        indices: Vec<u32>,
        group_plan: Option<Vec<dsperse::pipeline::GroupPayloadPart>>,
    },
}

pub(crate) struct PlannedSliceWork {
    pub run_uid: String,
    pub circuit: Arc<Circuit>,
    pub slice_id: String,
    pub run_source: RunSource,
    pub circuit_path: Option<String>,
    pub onnx_path: Option<String>,
    pub component_sha: Option<String>,
    pub payload: PlannedPayload,
}

const PLAN_MATERIALIZE_BATCH: usize = 16;

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

        self.plan_all_dslices(&run_uid, &circuit, RunSource::Api, submission.prove_pct)
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
    fn plan_group_contract(
        work: &dsperse::pipeline::SliceWork,
    ) -> Option<Vec<dsperse::pipeline::GroupPayloadPart>> {
        let ds = Self::group_dim_split(work)?;
        let circuit_path = match work.circuit_path.as_ref() {
            Some(p) => p,
            None => {
                warn!(slice = %work.slice_id, "group contract planning: no circuit path");
                return None;
            }
        };
        let backend = dsperse::backend::jstprove::JstproveBackend::new();
        let params = match backend.load_params(std::path::Path::new(circuit_path)) {
            Ok(Some(p)) => p,
            Ok(None) => {
                warn!(slice = %work.slice_id, circuit_path, "group contract planning: bundle has no params");
                return None;
            }
            Err(e) => {
                warn!(slice = %work.slice_id, circuit_path, error = %e, "group contract planning: params load failed");
                return None;
            }
        };
        let manifest_shapes: Vec<Vec<usize>> = work
            .named_inputs
            .iter()
            .map(|(_, t)| t.shape().to_vec())
            .collect();
        let contract: Vec<(String, Vec<usize>)> = params
            .inputs
            .iter()
            .map(|io| (io.name.clone(), io.shape.clone()))
            .collect();
        match dsperse::pipeline::plan_group_payload(&manifest_shapes, ds, &contract) {
            Ok(plan) => Some(plan),
            Err(e) => {
                warn!(slice = %work.slice_id, error = %e, "group contract planning: contract match failed");
                None
            }
        }
    }

    fn bundle_dispatch_mismatch(
        work: &dsperse::pipeline::SliceWork,
    ) -> Option<BundleDispatchMismatch> {
        let circuit_path = work.circuit_path.as_ref()?;
        let backend = dsperse::backend::jstprove::JstproveBackend::new();
        let params = match backend.load_params(std::path::Path::new(circuit_path)) {
            Ok(Some(p)) => p,
            _ => return None,
        };

        if let Some(ds) = Self::group_dim_split(work) {
            let bundle_expected: usize = params
                .inputs
                .iter()
                .map(|io| io.shape.iter().product::<usize>())
                .sum();
            let manifest_shapes: Vec<Vec<usize>> = work
                .named_inputs
                .iter()
                .map(|(_, t)| t.shape().to_vec())
                .collect();
            let contract: Vec<(String, Vec<usize>)> = params
                .inputs
                .iter()
                .map(|io| (io.name.clone(), io.shape.clone()))
                .collect();
            let per_request_actual =
                match dsperse::pipeline::plan_group_payload(&manifest_shapes, ds, &contract) {
                    Ok(plan) => plan
                        .iter()
                        .map(|part| match part {
                            dsperse::pipeline::GroupPayloadPart::Whole(i) => {
                                manifest_shapes[*i].iter().product::<usize>()
                            }
                            dsperse::pipeline::GroupPayloadPart::Split(i) => {
                                manifest_shapes[*i].iter().product::<usize>() / ds.dim_size
                                    * ds.elements_per_group
                            }
                        })
                        .sum::<usize>(),
                    Err(_) => 0,
                };
            let activation_expected: usize = contract
                .iter()
                .take_while(|(_, shape)| {
                    let n: usize = shape.iter().product();
                    manifest_shapes.iter().any(|m| {
                        m.iter().product::<usize>() == n
                            || m.iter().product::<usize>() / ds.dim_size * ds.elements_per_group
                                == n
                    })
                })
                .map(|(_, shape)| shape.iter().product::<usize>())
                .sum();
            return if per_request_actual > 0 && per_request_actual == activation_expected {
                None
            } else {
                Some(BundleDispatchMismatch {
                    bundle_expected,
                    per_request_actual,
                    strategy: "dim-split-group",
                })
            };
        }

        if let Some(ds) = &work.dim_split {
            let bundle_expected: usize = params
                .inputs
                .iter()
                .map(|io| io.shape.iter().product::<usize>())
                .sum();
            let k_chunk_size = ds.k_dim.div_ceil(ds.k_chunks.max(1));
            let per_request_actual = k_chunk_size.saturating_mul(1 + ds.n_dim);
            return if per_request_actual == bundle_expected {
                None
            } else {
                Some(BundleDispatchMismatch {
                    bundle_expected,
                    per_request_actual,
                    strategy: "dim-split",
                })
            };
        }

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

    pub(super) async fn plan_all_dslices(
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
        let slice_ids = match self.run_manager.circuit_work_ids(run_uid) {
            Ok(ids) => ids,
            Err(e) => {
                warn!(run_uid = %run_uid, error = %e, "failed to enumerate circuit work");
                self.teardown_run(run_uid).await;
                return;
            }
        };

        let slice_ids: Vec<String> = {
            let disabled: Option<HashSet<String>> =
                self.disabled_slices.get(&circuit.id).map(|m| {
                    m.iter()
                        .filter(|(_, &disabled_at)| {
                            self.current_block.saturating_sub(disabled_at)
                                < DISABLED_SLICE_REHAB_BLOCKS
                        })
                        .map(|(slice_id, _)| slice_id.clone())
                        .collect()
                });
            match disabled {
                Some(disabled) if !disabled.is_empty() => {
                    let (kept, skipped): (Vec<_>, Vec<_>) = slice_ids
                        .into_iter()
                        .partition(|slice_id| !disabled.contains(slice_id));
                    for slice_id in &skipped {
                        self.run_manager.mark_slice_failed(run_uid, slice_id);
                        self.run_manager.note_slice_skipped(run_uid, slice_id);
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
                _ => slice_ids,
            }
        };

        let mut planned_slices = 0usize;
        let mut planned_units = 0usize;
        let mut total_unsatisfiable = 0usize;

        for chunk in slice_ids.chunks(PLAN_MATERIALIZE_BATCH) {
            let mut works = Vec::with_capacity(chunk.len());
            for slice_id in chunk {
                match self.run_manager.circuit_work_for(run_uid, slice_id) {
                    Ok(w) => works.push(w),
                    Err(e) => {
                        warn!(run_uid = %run_uid, slice = %slice_id, error = %e, "failed to derive circuit work");
                        self.run_manager.mark_slice_failed(run_uid, slice_id);
                        self.run_manager.note_slice_skipped(run_uid, slice_id);
                    }
                }
            }

            // Bundle preflight reads each slice's manifest.msgpack and (for
            // WAI bundles) the slice's onnx graph; those are synchronous disk
            // reads that must not run on the async reactor. Move the
            // preflight loop into a blocking task and rejoin afterwards to
            // drive run_manager updates from the async context.
            let preflight_run_uid = run_uid.to_string();
            let preflight = tokio::task::spawn_blocking(move || {
                Self::preflight_work_items(&preflight_run_uid, works)
            })
            .await;
            let PreflightOutcome {
                kept,
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
                self.run_manager.note_slice_skipped(run_uid, slice_id);
            }
            total_unsatisfiable += unsatisfiable;

            for work in kept {
                let group_ds = Self::group_dim_split(&work).cloned();
                let group_contract_plan = if group_ds.is_some() {
                    Self::plan_group_contract(&work)
                } else {
                    None
                };
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

                let planned = if let Some(ds) = group_ds.as_ref() {
                    let sampled = Self::sample_tile_indices(
                        ds.num_groups,
                        clamped_prove_pct,
                        run_source,
                        run_uid,
                        &work.slice_id,
                    );
                    if sampled.is_empty() {
                        self.run_manager.mark_slice_failed(run_uid, &work.slice_id);
                        self.run_manager.note_slice_skipped(run_uid, &work.slice_id);
                        continue;
                    }
                    let expected: std::collections::HashSet<u32> =
                        sampled.iter().map(|&i| i as u32).collect();
                    if let Err(e) = self.run_manager.init_dim_split_counter(
                        run_uid,
                        &work.slice_id,
                        ds.num_groups,
                        expected,
                    ) {
                        warn!(run_uid = %run_uid, slice = %work.slice_id, error = %e, "init_dim_split_counter failed");
                        self.run_manager.mark_slice_failed(run_uid, &work.slice_id);
                        self.run_manager.note_slice_skipped(run_uid, &work.slice_id);
                        continue;
                    }
                    let group_plan = match group_contract_plan.clone() {
                        Some(payload_plan) => payload_plan,
                        None => {
                            warn!(run_uid = %run_uid, slice = %work.slice_id, "group contract planning failed");
                            self.run_manager.mark_slice_failed(run_uid, &work.slice_id);
                            self.run_manager.note_slice_skipped(run_uid, &work.slice_id);
                            continue;
                        }
                    };
                    let count = sampled.len();
                    let mut indices: Vec<u32> = sampled.into_iter().map(|i| i as u32).collect();
                    indices.sort_unstable();
                    (
                        PlannedPayload::DimSplit {
                            ds: ds.clone(),
                            indices,
                            group_plan: Some(group_plan),
                        },
                        count,
                    )
                } else if let Some(ds) = work
                    .dim_split
                    .as_ref()
                    .filter(|_| Self::dim_split_dispatch_enabled())
                    .filter(|ds| Self::is_weight_bound_dim_split(ds))
                {
                    match Self::plan_dim_split_work(
                        &mut self.run_manager,
                        run_uid,
                        &work.slice_id,
                        work.onnx_path.as_deref(),
                        ds,
                        &input_tensor,
                        run_source,
                        clamped_prove_pct,
                    ) {
                        Some(payload) => payload,
                        None => {
                            self.run_manager.mark_slice_failed(run_uid, &work.slice_id);
                            self.run_manager.note_slice_skipped(run_uid, &work.slice_id);
                            continue;
                        }
                    }
                } else if work.dim_split.is_some() {
                    self.run_manager.mark_slice_failed(run_uid, &work.slice_id);
                    self.run_manager.note_slice_skipped(run_uid, &work.slice_id);
                    continue;
                } else if let Some(ref tiling) = work.tiling {
                    match Self::plan_tiled_work(
                        &mut self.run_manager,
                        run_uid,
                        &work.slice_id,
                        tiling,
                        run_source,
                        clamped_prove_pct,
                        &input_tensor,
                        &named_inputs,
                    ) {
                        Some(payload) => payload,
                        None => {
                            self.teardown_run(run_uid).await;
                            return;
                        }
                    }
                } else {
                    (PlannedPayload::Single, 1)
                };

                let (payload, unit_count) = planned;
                self.dslice_plan.push_back(PlannedSliceWork {
                    run_uid: run_uid.to_string(),
                    circuit: Arc::clone(circuit),
                    slice_id: work.slice_id.clone(),
                    run_source,
                    circuit_path: work.circuit_path.clone(),
                    onnx_path: work.onnx_path.clone(),
                    component_sha: comp_sha.map(String::from),
                    payload,
                });
                planned_slices += 1;
                planned_units += unit_count;

                let ev_run = run_uid.to_string();
                let ev_slice = work.slice_id.clone();
                self.emit_event(move |ev| async move {
                    ev.emit_work_items_created(&ev_run, &ev_slice, unit_count)
                        .await;
                });
            }
        }

        if total_unsatisfiable > 0 {
            info!(
                run_uid = %run_uid,
                circuit_id = %circuit.id,
                unsatisfiable = total_unsatisfiable,
                "preflight filtered slices due to mismatched activation sizes or invalid metadata"
            );
        }

        if planned_slices == 0 {
            info!(run_uid = %run_uid, "no circuit slices to dispatch, completing run");
            self.finalize_combined_run(run_uid).await;
            return;
        }

        info!(
            run_uid = %run_uid,
            planned_slices,
            planned_units,
            "planned all circuit work items for combined run"
        );

        self.refill_dslice_queues();
        self.dispatch_notify.notify_one();
    }

    pub(super) fn refill_dslice_queues(&mut self) {
        if self.dslice_plan.is_empty() {
            return;
        }
        let queued = self.api_dslice_queue.len() + self.stacked_dslice_queue.len();
        let caps_sum: usize = self
            .performance_tracker
            .miner_capacities()
            .values()
            .copied()
            .sum();
        let low = caps_sum
            .saturating_mul(2)
            .clamp(DSLICE_QUEUE_LOW_WATERMARK, DSLICE_QUEUE_LOW_WATERMARK_MAX);
        if queued >= low {
            return;
        }
        let high = low.saturating_mul(2);
        let mut total = queued;
        while total < high {
            let Some(plan) = self.dslice_plan.pop_front() else {
                break;
            };
            if !self.run_manager.has_run(&plan.run_uid) {
                continue;
            }
            match self.materialize_planned(&plan) {
                Some(requests) if !requests.is_empty() => {
                    total += requests.len();
                    for request in requests {
                        match plan.run_source {
                            RunSource::Api => self.api_dslice_queue.push_back(request),
                            RunSource::Benchmark => self.stacked_dslice_queue.push_back(request),
                        }
                    }
                }
                _ => {
                    warn!(
                        run_uid = %plan.run_uid,
                        slice = %plan.slice_id,
                        "planned slice failed to materialize, marking failed"
                    );
                    self.run_manager
                        .mark_slice_failed(&plan.run_uid, &plan.slice_id);
                    self.run_manager
                        .note_slice_skipped(&plan.run_uid, &plan.slice_id);
                }
            }
        }
    }

    fn materialize_planned(&mut self, plan: &PlannedSliceWork) -> Option<Vec<DSliceRequest>> {
        let work = match self
            .run_manager
            .circuit_work_for(&plan.run_uid, &plan.slice_id)
        {
            Ok(w) => w,
            Err(e) => {
                warn!(
                    run_uid = %plan.run_uid,
                    slice = %plan.slice_id,
                    error = %e,
                    "failed to re-derive circuit work at refill"
                );
                return None;
            }
        };
        let mut input_tensor = work.input;
        let named_inputs = work.named_inputs;

        let norm_scale = self
            .dslice_input_scales
            .get(&(plan.run_uid.clone(), plan.slice_id.clone()))
            .copied()
            .filter(|&s| s > 1.0);
        if let Some(scale) = norm_scale {
            input_tensor.mapv_inplace(|v| v / scale);
        }

        match &plan.payload {
            PlannedPayload::Single => {
                let inputs_bytes = crate::tensor::input_data_payload(&input_tensor);
                Some(vec![DSliceRequest {
                    circuit: Arc::clone(&plan.circuit),
                    inputs: inputs_bytes,
                    request_type: RequestType::DSlice,
                    proof_system: plan.circuit.proof_system,
                    slice_num: plan.slice_id.clone(),
                    run_uid: plan.run_uid.clone(),
                    outputs: None,
                    is_tile: false,
                    tile_idx: None,
                    task_id: None,
                    run_source: plan.run_source,
                    retry_count: 0,
                    circuit_path: plan.circuit_path.clone(),
                    component_sha: plan.component_sha.clone(),
                }])
            }
            PlannedPayload::Tiled { tiling, indices } => {
                let wanted: std::collections::HashSet<usize> =
                    indices.iter().map(|&i| i as usize).collect();
                let prepared = Self::tiled_payloads(
                    &plan.run_uid,
                    &plan.slice_id,
                    tiling,
                    &input_tensor,
                    &named_inputs,
                    &wanted,
                )?;
                Some(
                    prepared
                        .into_iter()
                        .map(|(idx, tile_bytes)| {
                            Self::build_tile_request(
                                &plan.circuit,
                                &plan.slice_id,
                                &plan.run_uid,
                                tile_bytes,
                                idx as u32,
                                plan.run_source,
                                plan.circuit_path.as_deref(),
                                plan.component_sha.as_deref(),
                            )
                        })
                        .collect(),
                )
            }
            PlannedPayload::DimSplit {
                ds,
                indices,
                group_plan: Some(payload_plan),
            } if ds.weight_name.is_none() => {
                let wanted: std::collections::HashSet<usize> =
                    indices.iter().map(|&i| i as usize).collect();
                let tensors: Vec<&ndarray::ArrayD<f64>> =
                    named_inputs.iter().map(|(_, t)| t).collect();
                let payloads = match dsperse::pipeline::dim_split_group_payloads_planned(
                    &tensors,
                    payload_plan,
                    ds,
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(
                            run_uid = %plan.run_uid,
                            slice = %plan.slice_id,
                            error = %e,
                            "group payload construction failed"
                        );
                        return None;
                    }
                };
                Some(
                    payloads
                        .into_iter()
                        .enumerate()
                        .filter(|(idx, _)| wanted.contains(idx))
                        .map(|(idx, mut unit)| {
                            if let Some(scale) = norm_scale {
                                for v in unit.iter_mut() {
                                    *v /= scale;
                                }
                            }
                            let len = unit.len();
                            let arr = ndarray::ArrayD::from_shape_vec(ndarray::IxDyn(&[len]), unit)
                                .expect("1-D shape from vec");
                            Self::build_tile_request(
                                &plan.circuit,
                                &plan.slice_id,
                                &plan.run_uid,
                                crate::tensor::input_data_payload(&arr),
                                idx as u32,
                                plan.run_source,
                                plan.circuit_path.as_deref(),
                                plan.component_sha.as_deref(),
                            )
                        })
                        .collect(),
                )
            }
            PlannedPayload::DimSplit { ds, indices, .. } => {
                let wanted: std::collections::HashSet<usize> =
                    indices.iter().map(|&i| i as usize).collect();
                let prepared = Self::dim_split_payloads(
                    &plan.run_uid,
                    &plan.slice_id,
                    plan.onnx_path.as_deref(),
                    ds,
                    &input_tensor,
                    &wanted,
                )?;
                Some(
                    prepared
                        .into_iter()
                        .map(|(idx, unit_bytes)| {
                            Self::build_tile_request(
                                &plan.circuit,
                                &plan.slice_id,
                                &plan.run_uid,
                                unit_bytes,
                                idx as u32,
                                plan.run_source,
                                plan.circuit_path.as_deref(),
                                plan.component_sha.as_deref(),
                            )
                        })
                        .collect(),
                )
            }
        }
    }

    fn tiled_payloads(
        run_uid: &str,
        slice_id: &str,
        tiling: &dsperse::schema::tiling::TilingInfo,
        input_tensor: &ndarray::ArrayD<f64>,
        named_inputs: &[(String, ndarray::ArrayD<f64>)],
        wanted: &std::collections::HashSet<usize>,
    ) -> Option<Vec<(usize, bytes::Bytes)>> {
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
            match dsperse::pipeline::split_for_tiling(input_tensor, tiling) {
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

        let mut prepared: Vec<(usize, bytes::Bytes)> = Vec::with_capacity(wanted.len());
        match tiles_payload {
            TiledPayload::SingleInput(tiles) => {
                for (idx, tile) in tiles.into_iter().enumerate() {
                    if !wanted.contains(&idx) {
                        continue;
                    }
                    let tile_bytes = crate::tensor::input_data_payload(&tile.into_dyn());
                    prepared.push((idx, tile_bytes));
                }
            }
            TiledPayload::MultiInput(per_tile) => {
                for (idx, flat) in per_tile.into_iter().enumerate() {
                    if !wanted.contains(&idx) {
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

        Some(prepared)
    }

    fn plan_tiled_work(
        run_manager: &mut crate::incremental_runner::IncrementalRunManager,
        run_uid: &str,
        slice_id: &str,
        tiling: &dsperse::schema::tiling::TilingInfo,
        run_source: RunSource,
        prove_pct: f64,
        input_tensor: &ndarray::ArrayD<f64>,
        named_inputs: &[(String, ndarray::ArrayD<f64>)],
    ) -> Option<(PlannedPayload, usize)> {
        let sampled_indices =
            Self::sample_tile_indices(tiling.num_tiles, prove_pct, run_source, run_uid, slice_id);
        if sampled_indices.is_empty() {
            warn!(
                run_uid = %run_uid,
                slice = %slice_id,
                "prove_pct sampled zero tiles, aborting slice stage"
            );
            return None;
        }

        let prepared = Self::tiled_payloads(
            run_uid,
            slice_id,
            tiling,
            input_tensor,
            named_inputs,
            &sampled_indices,
        )?;

        let expected_indices: std::collections::HashSet<u32> =
            prepared.iter().map(|(idx, _)| *idx as u32).collect();
        if let Err(e) = run_manager.init_tile_counter(run_uid, slice_id, tiling, expected_indices) {
            warn!(run_uid = %run_uid, slice = %slice_id, error = %e, "init_tile_counter failed");
            return None;
        }

        let count = prepared.len();
        let mut indices: Vec<u32> = prepared.into_iter().map(|(idx, _)| idx as u32).collect();
        indices.sort_unstable();
        Some((
            PlannedPayload::Tiled {
                tiling: tiling.clone(),
                indices,
            },
            count,
        ))
    }

    fn dim_split_dispatch_enabled() -> bool {
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ENABLED.get_or_init(|| {
            std::env::var("SN2_DIM_SPLIT_DISPATCH")
                .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "on" | "yes"))
                .unwrap_or(false)
        })
    }

    fn is_weight_bound_dim_split(ds: &dsperse::schema::tiling::DimSplitInfo) -> bool {
        ds.weight_name.is_some() && ds.k_dim > 0 && ds.n_dim > 0
    }

    fn group_dim_split(
        work: &dsperse::pipeline::SliceWork,
    ) -> Option<&dsperse::schema::tiling::DimSplitInfo> {
        let ds = work.slice_meta.dim_split.as_ref()?;
        if work.named_inputs.len() > 1
            && ds.weight_name.is_none()
            && ds.num_groups > 0
            && ds.elements_per_group > 0
            && ds.num_groups * ds.elements_per_group == ds.dim_size
        {
            Some(ds)
        } else {
            None
        }
    }

    fn dim_split_payloads(
        run_uid: &str,
        slice_id: &str,
        onnx_path: Option<&str>,
        ds: &dsperse::schema::tiling::DimSplitInfo,
        input_tensor: &ndarray::ArrayD<f64>,
        wanted: &std::collections::HashSet<usize>,
    ) -> Option<Vec<(usize, bytes::Bytes)>> {
        let onnx_path = match onnx_path {
            Some(p) => p,
            None => {
                warn!(
                    run_uid = %run_uid,
                    slice = %slice_id,
                    "dim-split slice missing onnx_path, cannot bind chunk weights"
                );
                return None;
            }
        };
        let activations = match dsperse::pipeline::split_for_dim_split_dispatch(input_tensor, ds) {
            Ok(a) => a,
            Err(e) => {
                warn!(run_uid = %run_uid, slice = %slice_id, error = %e, "dim_split_bound_inputs failed");
                return None;
            }
        };
        let num_units = activations.len();
        if num_units == 0 {
            warn!(run_uid = %run_uid, slice = %slice_id, "dim-split produced zero units");
            return None;
        }

        let (full_weight, trans_b) = match dsperse::pipeline::dim_split_weight_and_transb(
            std::path::Path::new(onnx_path),
            ds,
        ) {
            Ok(w) => w,
            Err(e) => {
                warn!(run_uid = %run_uid, slice = %slice_id, error = %e, "dim_split_bound_inputs failed");
                return None;
            }
        };
        let k_chunks = ds.k_chunks.max(1);
        let weight_chunks: Vec<Vec<f64>> = (0..k_chunks)
            .map(|kc| {
                dsperse::pipeline::dim_split_weight_chunk(&full_weight, ds, kc, trans_b)
                    .into_iter()
                    .map(f64::from)
                    .collect()
            })
            .collect();

        let mut prepared: Vec<(usize, bytes::Bytes)> = Vec::with_capacity(wanted.len());
        for &idx in wanted {
            if idx >= num_units {
                warn!(
                    run_uid = %run_uid,
                    slice = %slice_id,
                    unit_idx = idx,
                    num_units,
                    "skipping dim-split unit: index out of range"
                );
                continue;
            }
            let mut unit = activations[idx].clone();
            unit.extend_from_slice(&weight_chunks[idx % k_chunks]);
            let len = unit.len();
            let unit_arr = match ndarray::ArrayD::from_shape_vec(ndarray::IxDyn(&[len]), unit) {
                Ok(arr) => arr,
                Err(e) => {
                    warn!(
                        run_uid = %run_uid,
                        slice = %slice_id,
                        unit_idx = idx,
                        error = %e,
                        "skipping dim-split unit: from_shape_vec rejected 1-D shape"
                    );
                    continue;
                }
            };
            let unit_bytes = crate::tensor::input_data_payload(&unit_arr);
            prepared.push((idx, unit_bytes));
        }

        if prepared.is_empty() {
            warn!(run_uid = %run_uid, slice = %slice_id, "no dim-split units survived staging");
            return None;
        }

        prepared.sort_unstable_by_key(|(idx, _)| *idx);
        Some(prepared)
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_dim_split_work(
        run_manager: &mut crate::incremental_runner::IncrementalRunManager,
        run_uid: &str,
        slice_id: &str,
        onnx_path: Option<&str>,
        ds: &dsperse::schema::tiling::DimSplitInfo,
        input_tensor: &ndarray::ArrayD<f64>,
        run_source: RunSource,
        prove_pct: f64,
    ) -> Option<(PlannedPayload, usize)> {
        if onnx_path.is_none() {
            warn!(
                run_uid = %run_uid,
                slice = %slice_id,
                "dim-split slice missing onnx_path, cannot bind chunk weights"
            );
            return None;
        }
        let activations = match dsperse::pipeline::split_for_dim_split_dispatch(input_tensor, ds) {
            Ok(a) => a,
            Err(e) => {
                warn!(run_uid = %run_uid, slice = %slice_id, error = %e, "dim_split_bound_inputs failed");
                return None;
            }
        };
        let num_units = activations.len();
        drop(activations);
        if num_units == 0 {
            warn!(run_uid = %run_uid, slice = %slice_id, "dim-split produced zero units");
            return None;
        }

        let sampled_indices =
            Self::sample_tile_indices(num_units, prove_pct, run_source, run_uid, slice_id);
        if sampled_indices.is_empty() {
            warn!(run_uid = %run_uid, slice = %slice_id, "prove_pct sampled zero dim-split units");
            return None;
        }

        let expected_indices: std::collections::HashSet<u32> =
            sampled_indices.iter().map(|&idx| idx as u32).collect();
        if let Err(e) =
            run_manager.init_dim_split_counter(run_uid, slice_id, num_units, expected_indices)
        {
            warn!(run_uid = %run_uid, slice = %slice_id, error = %e, "init_dim_split_counter failed");
            return None;
        }

        let count = sampled_indices.len();
        let mut indices: Vec<u32> = sampled_indices.into_iter().map(|idx| idx as u32).collect();
        indices.sort_unstable();
        Some((
            PlannedPayload::DimSplit {
                ds: ds.clone(),
                indices,
                group_plan: None,
            },
            count,
        ))
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
            let total_slices = slice_tiles.len();
            // Candidates for the disable-list write are slices that were
            // actually attempted in this run, returned without producing a
            // verified tile, and ended up marked failed. Slices that were
            // marked failed via the skip path (already-disabled, preflight
            // rejected) are excluded here so that the disabled_at clock for
            // already-disabled slices is not re-stamped to current_block on
            // every subsequent run. Re-stamping would defeat the
            // DISABLED_SLICE_REHAB_BLOCKS cooldown — a slice that ever
            // landed in disabled_slices would never age out because each
            // run would push its disabled_at forward.
            let candidates: Vec<String> = slice_tiles
                .into_keys()
                .filter(|slice_id| {
                    !self.run_manager.is_slice_skipped(run_uid, slice_id)
                        && self.run_manager.is_slice_failed(run_uid, slice_id)
                        && self.run_manager.verified_tile_count(run_uid, slice_id) == 0
                })
                .collect();
            // The disable-list write is suppressed only when at least one
            // slice was actually attempted and every attempted slice failed
            // with zero verified tiles. That signal is characteristic of a
            // validator-side network or chain event (mass QUIC reconnect,
            // RPC stall) and would otherwise trap the validator into a
            // permanent no-dispatch loop. Slices that never reached the
            // miner — already-disabled entries or preflight rejections —
            // are tracked via note_slice_skipped at the call sites and
            // already excluded from the candidates filter above, so
            // candidates.len() is exactly the attempted-and-failed count.
            let skipped = self.run_manager.skipped_slice_count(run_uid);
            let attempted = total_slices.saturating_sub(skipped);
            let run_wide_failure = attempted > 0 && candidates.len() == attempted;
            if run_wide_failure {
                warn!(
                    run_uid = %run_uid,
                    circuit_id = %circuit_id,
                    total_slices,
                    skipped,
                    attempted,
                    "every attempted slice failed with zero verified tiles, treating as run-wide failure and skipping disable-list write"
                );
            } else if !candidates.is_empty() {
                let block = self.current_block;
                let entry = self.disabled_slices.entry(circuit_id.clone()).or_default();
                let mut inserted = 0usize;
                for slice_id in &candidates {
                    if entry.insert(slice_id.clone(), block).is_none() {
                        inserted += 1;
                    }
                }
                if inserted > 0 {
                    info!(
                        circuit_id = %circuit_id,
                        newly_disabled = inserted,
                        total_disabled_for_circuit = entry.len(),
                        rehab_blocks = DISABLED_SLICE_REHAB_BLOCKS,
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
        if self.config.disable_benchmark {
            return;
        }
        let benchmark_runs = self.run_manager.benchmark_run_uids().len();
        if benchmark_runs >= MAX_CONCURRENT_BENCHMARK_RUNS {
            return;
        }
        if benchmark_runs > 0 {
            let supply = self.stacked_dslice_queue.len() + self.dslice_plan.len();
            if supply >= DSLICE_QUEUE_LOW_WATERMARK {
                return;
            }
            match crate::performance::host_memory_available_ratio() {
                Some(r) if r >= EXTRA_RUN_MIN_AVAIL_MEM_RATIO => {}
                _ => return,
            }
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
        self.plan_all_dslices(&run_uid, &circuit, RunSource::Benchmark, 1.0)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::ValidatorLoop;
    use dsperse::schema::tiling::{DimSplitInfo, DimSplitKind};

    #[test]
    fn group_dim_split_requires_multi_input_and_covering_groups() {
        use dsperse::pipeline::SliceWork;
        use dsperse::schema::metadata::RunSliceMetadata;

        fn work(named: usize, ds: Option<DimSplitInfo>) -> SliceWork {
            let named_inputs = (0..named)
                .map(|i| {
                    (
                        format!("t{i}"),
                        ndarray::ArrayD::from_elem(ndarray::IxDyn(&[4]), 1.0),
                    )
                })
                .collect();
            SliceWork {
                slice_id: "slice_0".to_string(),
                input: ndarray::ArrayD::from_elem(ndarray::IxDyn(&[4]), 1.0),
                named_inputs,
                backend: Default::default(),
                use_circuit: true,
                tiling: None,
                channel_split: None,
                dim_split: None,
                circuit_path: None,
                onnx_path: None,
                slice_meta: RunSliceMetadata {
                    dim_split: ds,
                    ..Default::default()
                },
            }
        }

        let group = DimSplitInfo {
            split_kind: DimSplitKind::BatchDim,
            weight_name: None,
            split_dim: 0,
            dim_size: 4,
            num_groups: 2,
            elements_per_group: 2,
            ..Default::default()
        };
        assert!(ValidatorLoop::group_dim_split(&work(2, Some(group.clone()))).is_some());
        assert!(ValidatorLoop::group_dim_split(&work(1, Some(group.clone()))).is_none());
        let weight_bound = DimSplitInfo {
            weight_name: Some("w".to_string()),
            ..group.clone()
        };
        assert!(ValidatorLoop::group_dim_split(&work(2, Some(weight_bound))).is_none());
        let uncovered = DimSplitInfo {
            num_groups: 3,
            ..group
        };
        assert!(ValidatorLoop::group_dim_split(&work(2, Some(uncovered))).is_none());
    }

    #[test]
    fn weight_bound_dim_split_requires_full_matmul_metadata() {
        let matmul = DimSplitInfo {
            split_kind: DimSplitKind::MatMulOutputDim,
            weight_name: Some("onnx::MatMul_4211".to_string()),
            k_dim: 384,
            n_dim: 1536,
            k_chunks: 2,
            ..Default::default()
        };
        assert!(ValidatorLoop::is_weight_bound_dim_split(&matmul));

        let head_dim = DimSplitInfo {
            split_kind: DimSplitKind::HeadDim,
            weight_name: None,
            ..Default::default()
        };
        assert!(!ValidatorLoop::is_weight_bound_dim_split(&head_dim));

        let weightless = DimSplitInfo {
            weight_name: None,
            k_dim: 384,
            n_dim: 1536,
            ..Default::default()
        };
        assert!(!ValidatorLoop::is_weight_bound_dim_split(&weightless));

        let zero_dims = DimSplitInfo {
            weight_name: Some("w".to_string()),
            k_dim: 0,
            n_dim: 0,
            ..Default::default()
        };
        assert!(!ValidatorLoop::is_weight_bound_dim_split(&zero_dims));
    }
}
