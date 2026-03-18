use std::time::{Duration, Instant};

use anyhow::Result;
use sn2_types::*;
use tracing::{debug, info, warn};

use super::ValidatorLoop;
use crate::relay::DsperseSubmission;

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

        let incremental = match dsperse::pipeline::IncrementalRun::new(&slices_dir, input_tensor) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, circuit = %circuit.id, "failed to create IncrementalRun");
                self.send_submit_error(
                    submission.request_id,
                    "failed to initialize incremental run",
                )
                .await;
                return;
            }
        };

        let run_uid = uuid::Uuid::new_v4().to_string();
        info!(run_uid = %run_uid, circuit = %circuit.id, "started incremental run");

        self.run_manager.start_run(
            run_uid.clone(),
            circuit.id.clone(),
            circuit.metadata.name.clone(),
            RunSource::Api,
            submission.request_id,
            Some(incremental),
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

        self.enqueue_next_dslice(&run_uid, &circuit).await;
    }

    pub(super) async fn enqueue_next_dslice(&mut self, run_uid: &str, circuit: &Circuit) {
        let slices_dir = circuit.paths.base_path.join("slices");
        loop {
            let mut slice_info = match self.run_manager.next_slice(run_uid) {
                Ok(Some(info)) => info,
                Ok(None) => {
                    debug!(run_uid = %run_uid, "no next slice available");
                    return;
                }
                Err(e) => {
                    warn!(run_uid = %run_uid, error = %e, "next_slice failed");
                    return;
                }
            };

            {
                let dir = slices_dir.clone();
                let sid = slice_info.slice_id.clone();
                let result = tokio::task::spawn_blocking(move || {
                    sn2_circuit_store::ensure_slice_extracted(&dir, &sid)
                })
                .await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        warn!(run_uid = %run_uid, slice = %slice_info.slice_id, error = %e, "failed to extract dslice");
                        self.teardown_run(run_uid).await;
                        return;
                    }
                    Err(e) => {
                        warn!(run_uid = %run_uid, slice = %slice_info.slice_id, error = %e, "dslice extraction task panicked");
                        self.teardown_run(run_uid).await;
                        return;
                    }
                }
            }

            if !slice_info.use_circuit {
                let onnx_path = match slice_info.onnx_path {
                    Some(ref p) => p.clone(),
                    None => {
                        warn!(run_uid = %run_uid, slice = %slice_info.slice_id, "non-circuit slice has no onnx_path");
                        self.teardown_run(run_uid).await;
                        return;
                    }
                };
                for (name, arr) in &slice_info.named_inputs {
                    let nan_c = arr.iter().filter(|v| v.is_nan()).count();
                    let inf_c = arr.iter().filter(|v| v.is_infinite()).count();
                    let max_abs = arr.iter().fold(0.0_f64, |m, v| m.max(v.abs()));
                    let f32_overflow = arr.iter().filter(|&&v| v.abs() > f32::MAX as f64).count();
                    let elems = arr.len();
                    info!(
                        run_uid = %run_uid,
                        slice = %slice_info.slice_id,
                        input_name = %name,
                        shape = ?arr.shape(),
                        elems = elems,
                        nan = nan_c,
                        inf = inf_c,
                        max_abs = max_abs,
                        f32_overflow = f32_overflow,
                        "ONNX slice input tensor stats"
                    );
                }
                let output_tensor = match Self::run_onnx_slice_inference(
                    run_uid,
                    &slice_info.slice_id,
                    &onnx_path,
                    &slice_info.named_inputs,
                    &slice_info.input_tensor,
                )
                .await
                {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(run_uid = %run_uid, slice = %slice_info.slice_id, error = %e, "ONNX slice inference failed");
                        self.teardown_run(run_uid).await;
                        return;
                    }
                };
                match self.run_manager.apply_result_tensor(
                    run_uid,
                    &slice_info.slice_id,
                    output_tensor,
                ) {
                    Ok(true) => {
                        sn2_circuit_store::cleanup_extracted_slice(
                            &slices_dir,
                            &slice_info.slice_id,
                        );
                        info!(run_uid = %run_uid, "incremental run complete after ONNX slice");
                        let active_run = self.run_manager.remove_run(run_uid);
                        if let Some(ref run) = active_run {
                            self.report_dsperse_completion(run);

                            self.spawn_emit_run_complete(run, true);
                        }
                        let notify_circuit_id = active_run
                            .as_ref()
                            .map(|r| r.circuit_id.as_str())
                            .unwrap_or_default()
                            .to_string();
                        self.relay_set_request_result(
                            run_uid,
                            serde_json::json!({"run_uid": run_uid, "status": "complete"}),
                        )
                        .await;
                        self.relay_send_notification(
                            "subnet-2.batch_completed",
                            serde_json::json!({
                                "run_uid": run_uid,
                                "circuit_id": notify_circuit_id,
                                "status": "completed",
                            }),
                        )
                        .await;
                        return;
                    }
                    Ok(false) => {
                        sn2_circuit_store::cleanup_extracted_slice(
                            &slices_dir,
                            &slice_info.slice_id,
                        );
                        continue;
                    }
                    Err(e) => {
                        sn2_circuit_store::cleanup_extracted_slice(
                            &slices_dir,
                            &slice_info.slice_id,
                        );
                        warn!(run_uid = %run_uid, error = %e, "apply_result failed for ONNX slice");
                        self.teardown_run(run_uid).await;
                        return;
                    }
                }
            }

            let run_source = self
                .run_manager
                .get_run_source(run_uid)
                .unwrap_or(RunSource::Benchmark);

            if slice_info.input_tensor.iter().any(|v| !v.is_finite()) {
                warn!(
                    run_uid = %run_uid,
                    slice = %slice_info.slice_id,
                    "circuit slice input contains non-finite values, aborting run"
                );
                self.teardown_run(run_uid).await;
                return;
            }

            let input_max_abs = slice_info
                .input_tensor
                .iter()
                .fold(0.0_f64, |m, v| m.max(v.abs()));
            if input_max_abs > 1.0 {
                slice_info.input_tensor.mapv_inplace(|v| v / input_max_abs);
                slice_info.inputs_json = serde_json::json!({
                    "input_data": crate::tensor::arrayd_to_json(&slice_info.input_tensor)
                });
                self.dslice_input_scales.insert(
                    (run_uid.to_string(), slice_info.slice_id.clone()),
                    input_max_abs,
                );
                info!(
                    run_uid = %run_uid,
                    slice = %slice_info.slice_id,
                    input_max_abs,
                    "normalized circuit slice inputs to [-1, 1]"
                );
            }

            if let Some(ref tiling) = slice_info.tiling {
                let input_4d = match slice_info
                    .input_tensor
                    .into_dimensionality::<ndarray::Ix4>()
                {
                    Ok(arr) => arr,
                    Err(e) => {
                        warn!(
                            run_uid = %run_uid,
                            slice = %slice_info.slice_id,
                            error = %e,
                            "tiled slice requires 4D input"
                        );
                        self.teardown_run(run_uid).await;
                        return;
                    }
                };
                let tiles = match dsperse::pipeline::split_into_tiles(&input_4d, tiling) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(
                            run_uid = %run_uid,
                            slice = %slice_info.slice_id,
                            error = %e,
                            "split_into_tiles failed"
                        );
                        self.teardown_run(run_uid).await;
                        return;
                    }
                };

                self.dispatch_tiled_slice(
                    run_uid,
                    circuit,
                    &slice_info.slice_id,
                    slice_info.circuit_path.as_deref(),
                    tiling,
                    &slices_dir,
                    tiles,
                    run_source,
                )
                .await;
                return;
            }

            let request = DSliceRequest {
                circuit: circuit.clone(),
                inputs: slice_info.inputs_json,
                request_type: RequestType::DSlice,
                proof_system: circuit.proof_system,
                slice_num: slice_info.slice_id.clone(),
                run_uid: run_uid.to_string(),
                outputs: None,
                is_tile: false,
                tile_idx: None,
                task_id: None,
                run_source,
                retry_count: 0,
                circuit_path: slice_info.circuit_path.clone(),
            };
            match request.run_source {
                RunSource::Api => self.api_dslice_queue.push_back(request),
                RunSource::Benchmark => self.stacked_dslice_queue.push_back(request),
            };
            return;
        }
    }

    pub(super) async fn run_onnx_slice_inference(
        run_uid: &str,
        slice_id: &str,
        onnx_path: &str,
        named_inputs: &[(String, ndarray::ArrayD<f64>)],
        input_tensor: &ndarray::ArrayD<f64>,
    ) -> Result<ndarray::ArrayD<f64>> {
        let inference_result = if named_inputs.len() > 1 {
            let inputs: Vec<(String, Vec<f64>, Vec<usize>)> = named_inputs
                .iter()
                .map(|(name, arr)| {
                    (
                        name.clone(),
                        arr.iter().copied().collect(),
                        arr.shape().to_vec(),
                    )
                })
                .collect();
            let onnx = onnx_path.to_string();
            tokio::task::spawn_blocking(move || {
                let refs: Vec<(&str, Vec<f64>, Vec<usize>)> = inputs
                    .iter()
                    .map(|(n, d, s)| (n.as_str(), d.clone(), s.clone()))
                    .collect();
                dsperse::backend::onnx::run_inference_multi(std::path::Path::new(&onnx), &refs)
            })
            .await
        } else {
            let input_flat: Vec<f64> = input_tensor.iter().copied().collect();
            let input_shape: Vec<usize> = input_tensor.shape().to_vec();
            let onnx = onnx_path.to_string();
            tokio::task::spawn_blocking(move || {
                dsperse::backend::onnx::run_inference(
                    std::path::Path::new(&onnx),
                    &input_flat,
                    &input_shape,
                )
            })
            .await
        };
        let (output_data, output_shape) = match inference_result {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                return Err(anyhow::anyhow!("ONNX inference failed: {e}"));
            }
            Err(e) => {
                return Err(anyhow::anyhow!("ONNX inference task panicked: {e}"));
            }
        };
        let output_tensor =
            ndarray::ArrayD::from_shape_vec(ndarray::IxDyn(&output_shape), output_data).map_err(
                |e| anyhow::anyhow!("ONNX output shape mismatch (shape={output_shape:?}): {e}"),
            )?;
        let nan_count = output_tensor.iter().filter(|v| v.is_nan()).count();
        let inf_count = output_tensor.iter().filter(|v| v.is_infinite()).count();
        let total_elems = output_tensor.len();
        if nan_count > 0 || inf_count > 0 {
            warn!(
                run_uid = %run_uid,
                slice = %slice_id,
                output_shape = ?output_tensor.shape(),
                nan_count = nan_count,
                inf_count = inf_count,
                total_elems = total_elems,
                "ONNX output contains non-finite values"
            );
            anyhow::bail!(
                "ONNX output contains non-finite values (nan={nan_count}, inf={inf_count}, total={total_elems})"
            );
        }
        info!(
            run_uid = %run_uid,
            slice = %slice_id,
            output_shape = ?output_tensor.shape(),
            total_elems = total_elems,
            "ran ONNX inference for non-circuit slice"
        );
        Ok(output_tensor)
    }

    pub(super) async fn dispatch_tiled_slice(
        &mut self,
        run_uid: &str,
        circuit: &Circuit,
        slice_id: &str,
        circuit_path: Option<&str>,
        tiling: &dsperse::schema::tiling::TilingInfo,
        slices_dir: &std::path::Path,
        tiles: Vec<ndarray::Array4<f64>>,
        run_source: RunSource,
    ) {
        let num_tiles = tiles.len();

        if num_tiles == 0 {
            let slice_path = slices_dir.join(slice_id);
            sn2_verify::evict_circuit_cache(&slice_path.to_string_lossy());
            sn2_circuit_store::cleanup_extracted_slice(slices_dir, slice_id);
            warn!(
                run_uid = %run_uid,
                slice = %slice_id,
                "split_into_tiles returned no tiles"
            );
            self.teardown_run(run_uid).await;
            return;
        }

        if run_source == RunSource::Api {
            info!(
                run_uid = %run_uid,
                slice = %slice_id,
                num_tiles,
                "API request: dispatching single tile for proven inference"
            );
            let tile = &tiles[0];
            let tile_json = serde_json::json!({
                "input_data": crate::tensor::arrayd_to_json(&tile.clone().into_dyn())
            });
            let request = DSliceRequest {
                circuit: circuit.clone(),
                inputs: tile_json,
                request_type: RequestType::DSlice,
                proof_system: circuit.proof_system,
                slice_num: slice_id.to_string(),
                run_uid: run_uid.to_string(),
                outputs: None,
                is_tile: false,
                tile_idx: None,
                task_id: None,
                run_source,
                retry_count: 0,
                circuit_path: circuit_path.map(String::from),
            };
            self.api_dslice_queue.push_back(request);

            {
                let uid = run_uid.to_string();
                let snum = slice_id.to_string();
                self.emit_event(move |ev| async move {
                    ev.emit_work_items_created(&uid, &snum, 1).await;
                });
            }

            return;
        }

        info!(
            run_uid = %run_uid,
            slice = %slice_id,
            num_tiles,
            "dispatching spatial tiles"
        );

        if let Err(e) = self
            .run_manager
            .init_tile_buffer(run_uid, slice_id, tiling.clone())
        {
            warn!(
                run_uid = %run_uid,
                slice = %slice_id,
                error = %e,
                "init_tile_buffer failed"
            );
            self.teardown_run(run_uid).await;
            return;
        }

        for (idx, tile) in tiles.into_iter().enumerate() {
            let tile_json = serde_json::json!({
                "input_data": crate::tensor::arrayd_to_json(&tile.into_dyn())
            });
            let request = DSliceRequest {
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
            };
            self.stacked_dslice_queue.push_back(request);
        }

        {
            let uid = run_uid.to_string();
            let snum = slice_id.to_string();
            let nt = num_tiles;
            self.emit_event(move |ev| async move {
                ev.emit_work_items_created(&uid, &snum, nt).await;
            });
        }
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
        let idx = rand::Rng::gen_range(&mut rand::thread_rng(), 0..dsperse_circuits.len());
        let circuit = &dsperse_circuits[idx];

        let schema = match &circuit.metadata.input_schema {
            Some(s) if !s.is_empty() => s.clone(),
            _ => {
                warn!(circuit = %circuit.id, "dsperse circuit has no input_schema, cannot benchmark");
                return;
            }
        };

        let shape: Vec<usize> = match schema
            .get("shape")
            .and_then(|v| v.as_array())
            .and_then(|dims| {
                dims.iter()
                    .map(|d| d.as_u64().map(|v| v as usize))
                    .collect::<Option<Vec<_>>>()
            })
            .filter(|s| !s.is_empty() && s.iter().all(|&d| d > 0))
        {
            Some(s) => s,
            None => {
                warn!(circuit = %circuit.id, "cannot derive tensor shape from input_schema");
                return;
            }
        };

        let mut rng = rand::thread_rng();
        let input = ndarray::ArrayD::from_shape_fn(ndarray::IxDyn(&shape), |_| {
            rand::Rng::gen_range(&mut rng, 0.0_f64..1.0)
        });
        let slices_dir = circuit.paths.base_path.join("slices");

        let incremental = match dsperse::pipeline::IncrementalRun::new(&slices_dir, input) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, circuit = %circuit.id, "failed to create benchmark IncrementalRun");
                self.dsperse_benchmark_backoff_until = Instant::now() + Duration::from_secs(60);
                return;
            }
        };

        let run_uid = uuid::Uuid::new_v4().to_string();
        info!(run_uid = %run_uid, circuit = %circuit.id, name = %circuit.metadata.name, "started dsperse benchmark run");

        self.run_manager.start_run(
            run_uid.clone(),
            circuit.id.clone(),
            circuit.metadata.name.clone(),
            RunSource::Benchmark,
            None,
            Some(incremental),
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
        self.enqueue_next_dslice(&run_uid, &circuit).await;
        self.dispatch_notify.notify_one();
    }
}
