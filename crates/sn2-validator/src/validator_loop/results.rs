use sn2_types::*;
use tracing::{info, warn};

use super::{event_slice_num, RetryPayload, ValidatorLoop};
use crate::incremental_runner::SliceArtifact;
use crate::metrics_server as metrics;
use crate::pow_manager::PowItem;
use crate::relay::{FRAME_PROOF_RESULT, FRAME_SUBMIT_RESULT};

impl ValidatorLoop {
    pub(super) fn record_verified_score(
        &mut self,
        uid: u16,
        response: &MinerResponse,
        was_at_capacity: bool,
    ) {
        let elapsed = response.response_time;
        self.performance_tracker
            .record(uid, true, elapsed, was_at_capacity);
        let previous_score = self.score_manager.get_score(uid);
        self.score_manager.update_score(
            uid,
            true,
            elapsed,
            VALIDATOR_REQUEST_TIMEOUT_SECONDS as f64,
            0.0,
            self.config.metagraph.n,
        );
        metrics::record_response(true, elapsed);

        let n = self.config.metagraph.n.max(1) as f64;
        self.pow_manager.push(PowItem {
            miner_uid: uid,
            validator_uid: self.config.user_uid,
            verified: true,
            response_time: elapsed,
            proof_size: response.proof_size as u64,
            previous_score,
            maximum_score: 1.0 / n,
            maximum_response_time: VALIDATOR_REQUEST_TIMEOUT_SECONDS as f64,
            minimum_response_time: 0.0,
            block_number: self.config.metagraph.block,
        });
    }

    pub(super) async fn handle_pow_success(&mut self, response: &MinerResponse) {
        let rescaled_outputs: Vec<f64> = match &response.computed_outputs {
            Some(v) => serde_json::from_value(v.clone()).unwrap_or_default(),
            None => {
                warn!("PoW response missing computed_outputs");
                return;
            }
        };

        let expected_len = POW_OUTPUT_STRIDE * POW_NUM_OUTPUT_ARRAYS;
        if rescaled_outputs.len() < expected_len {
            warn!(
                outputs_len = rescaled_outputs.len(),
                expected = expected_len,
                "PoW witness outputs too short"
            );
            return;
        }

        let score_slice =
            &rescaled_outputs[POW_SCORES_OFFSET..POW_SCORES_OFFSET + POW_OUTPUT_STRIDE];
        let uid_slice = &rescaled_outputs[POW_UIDS_OFFSET..POW_UIDS_OFFSET + POW_OUTPUT_STRIDE];

        let mut valid_uids = Vec::with_capacity(POW_OUTPUT_STRIDE);
        let mut valid_scores = Vec::with_capacity(POW_OUTPUT_STRIDE);
        for (uid_f, &score) in uid_slice.iter().zip(score_slice.iter()) {
            if !uid_f.is_finite() || uid_f.round() < 0.0 || uid_f.round() > u16::MAX as f64 {
                continue;
            }
            if !score.is_finite() {
                continue;
            }
            valid_uids.push(uid_f.round() as u16);
            valid_scores.push(score);
        }

        self.score_manager
            .apply_pow_scores(&valid_uids, &valid_scores);

        info!(
            batch = POW_OUTPUT_STRIDE,
            "applied PoW-derived scores from verified witness"
        );

        if let Err(e) = self.score_manager.save() {
            warn!(error = %e, "saving scores after PoW update");
        }
    }

    pub(super) async fn finalize_api_run(&mut self, run_uid: &str, slice_num: &str) {
        let scale_key = (run_uid.to_string(), slice_num.to_string());
        let scale = self.dslice_input_scales.remove(&scale_key);
        self.dslice_input_scales
            .retain(|(uid, _), _| uid != run_uid);

        self.cleanup_previous_slice(run_uid);

        info!(
            run_uid = %run_uid,
            slice = %slice_num,
            "API request: single proven tile complete, finalizing run"
        );
        let mut active_run = self.run_manager.remove_run(run_uid);

        if let Some(input_scale) = scale {
            if let Some(ref mut run) = active_run {
                for artifact in &mut run.artifacts {
                    if let Some(ref mut outputs) = artifact.computed_outputs {
                        if let Ok(mut tensor) = crate::tensor::json_to_arrayd(outputs) {
                            tensor.mapv_inplace(|v| v * input_scale);
                            *outputs = crate::tensor::arrayd_to_json(&tensor);
                        }
                    }
                }
                info!(
                    run_uid = %run_uid,
                    scale = input_scale,
                    "denormalized API run artifact outputs"
                );
            }
        }

        if let Some(ref run) = active_run {
            self.report_dsperse_completion(run);
            self.spawn_emit_run_complete(run, true);
        }
        self.spawn_artifact_upload(run_uid, &mut active_run, None);
        self.notify_run_completed(run_uid, &active_run).await;
    }

    pub(super) async fn denormalize_and_apply_output(
        &mut self,
        run_uid: &str,
        slice_num: &str,
        computed: &serde_json::Value,
    ) {
        let scale_key = (run_uid.to_string(), slice_num.to_string());
        let scale = self.dslice_input_scales.remove(&scale_key);

        let mut tensor = match crate::tensor::json_to_arrayd(computed) {
            Ok(t) => t,
            Err(e) => {
                warn!(run_uid = %run_uid, slice = %slice_num, error = %e, "output tensor conversion failed, removing run");
                self.teardown_run(run_uid).await;
                return;
            }
        };

        if let Some(scale) = scale {
            tensor.mapv_inplace(|v| v * scale);
            info!(
                run_uid = %run_uid,
                slice = %slice_num,
                scale,
                "denormalized circuit output"
            );
        }

        self.apply_dslice_result_tensor(run_uid, slice_num, tensor)
            .await;
    }

    pub(super) fn attempt_retry(&mut self, retry_payload: RetryPayload, next_retry: u32) -> bool {
        match retry_payload {
            RetryPayload::Rwr(mut rwr) => {
                rwr.retry_count = next_retry;
                self.rwr_queue.push_back(rwr);
                self.dispatch_notify.notify_one();
                true
            }
            RetryPayload::DSlice(mut dslice) => {
                if self.run_manager.has_run(&dslice.run_uid) {
                    dslice.retry_count = next_retry;
                    match dslice.run_source {
                        RunSource::Api => self.api_dslice_queue.push_back(*dslice),
                        RunSource::Benchmark => self.stacked_dslice_queue.push_back(*dslice),
                    }
                    self.dispatch_notify.notify_one();
                    true
                } else {
                    false
                }
            }
            RetryPayload::None => false,
        }
    }

    async fn handle_dslice_max_retries(
        &mut self,
        run_uid: &Option<String>,
        slice_num: &Option<String>,
        is_tile: bool,
        tile_idx: Option<u32>,
        reason: &str,
    ) {
        if let Some(run_uid) = run_uid {
            if let Some(snum) = slice_num {
                let ruid = run_uid.clone();
                let event_snum = event_slice_num(snum, is_tile, tile_idx);
                let err = reason.to_string();
                self.emit_event(move |ev| async move {
                    ev.emit_slice_failed(&ruid, &event_snum, &err).await;
                });
            }
            warn!(run_uid = %run_uid, "dslice max retries exceeded, removing run");
            if self.run_manager.get_run_source(run_uid) == Some(RunSource::Api) {
                self.relay_set_request_result(
                    run_uid,
                    serde_json::json!({
                        "run_uid": run_uid,
                        "status": "failed",
                        "error": "max retries exceeded",
                    }),
                )
                .await;
            }
            self.teardown_run(run_uid).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_dslice_success(
        &mut self,
        run_uid: &Option<String>,
        slice_num: &Option<String>,
        is_tile: bool,
        _task_id: Option<&str>,
        tile_idx: Option<u32>,
        response: &MinerResponse,
        verification_time: f64,
    ) {
        let run_uid = match run_uid {
            Some(r) => r.clone(),
            None => return,
        };
        let slice_num = match slice_num {
            Some(s) => s.clone(),
            None => return,
        };

        let proof_str = response.proof_content.as_ref().and_then(|v| v.as_str());

        if self.run_manager.is_evicted(&run_uid) {
            return;
        }

        if !self.run_manager.has_run(&run_uid) {
            return;
        }

        self.run_manager.push_artifact(
            &run_uid,
            SliceArtifact {
                slice_num: slice_num.clone(),
                proof_system: response.proof_system,
                proof_hex: proof_str.map(|s| s.to_string()),
                witness_hex: response.witness.clone(),
                computed_outputs: response.computed_outputs.clone(),
                tile_idx,
                response_time: response.response_time,
                verification_time,
            },
        );

        if self.run_manager.get_run_source(&run_uid) == Some(RunSource::Api) {
            self.finalize_api_run(&run_uid, &slice_num).await;
            return;
        }

        if is_tile {
            let tile_idx = match tile_idx {
                Some(idx) => idx,
                None => {
                    warn!(run_uid = %run_uid, slice = %slice_num, "tile response missing tile_idx, removing run");
                    self.teardown_run(&run_uid).await;
                    return;
                }
            };

            let computed = response
                .computed_outputs
                .as_ref()
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            let tile_output = match crate::tensor::json_to_arrayd(&computed) {
                Ok(t) => t,
                Err(e) => {
                    warn!(
                        run_uid = %run_uid,
                        slice = %slice_num,
                        tile_idx = tile_idx,
                        error = %e,
                        "tile output tensor conversion failed, removing run"
                    );
                    self.teardown_run(&run_uid).await;
                    return;
                }
            };

            use crate::incremental_runner::TileBufferOutcome;
            match self
                .run_manager
                .buffer_tile_result(&run_uid, &slice_num, tile_idx, tile_output)
            {
                TileBufferOutcome::Waiting => return,
                TileBufferOutcome::Ready(mut full_output) => {
                    let scale_key = (run_uid.clone(), slice_num.clone());
                    if let Some(scale) = self.dslice_input_scales.remove(&scale_key) {
                        full_output.mapv_inplace(|v| v * scale);
                        info!(
                            run_uid = %run_uid,
                            slice = %slice_num,
                            scale,
                            "denormalized tiled circuit output"
                        );
                    }
                    self.apply_dslice_result_tensor(&run_uid, &slice_num, full_output)
                        .await;
                }
                TileBufferOutcome::Failed(reason) => {
                    warn!(
                        run_uid = %run_uid,
                        slice = %slice_num,
                        error = %reason,
                        "tile buffering failed, removing run"
                    );
                    self.teardown_run(&run_uid).await;
                }
            }
            return;
        }

        let computed = response
            .computed_outputs
            .as_ref()
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        self.denormalize_and_apply_output(&run_uid, &slice_num, &computed)
            .await;
    }

    pub(super) async fn apply_dslice_result_tensor(
        &mut self,
        run_uid: &str,
        slice_num: &str,
        output: ndarray::ArrayD<f64>,
    ) {
        match self
            .run_manager
            .apply_result_tensor(run_uid, slice_num, output)
        {
            Ok(is_complete) => {
                if is_complete {
                    self.cleanup_previous_slice(run_uid);
                    info!(run_uid = %run_uid, "incremental run complete");

                    let final_output = self.run_manager.final_output_json(run_uid);
                    let mut active_run = self.run_manager.remove_run(run_uid);

                    if let Some(ref run) = active_run {
                        self.report_dsperse_completion(run);
                        self.spawn_emit_run_complete(run, true);
                    }

                    self.spawn_artifact_upload(run_uid, &mut active_run, final_output);
                    self.notify_run_completed(run_uid, &active_run).await;
                } else {
                    self.enqueue_next_dslice_from_run(run_uid).await;
                }
            }
            Err(e) => {
                warn!(run_uid = %run_uid, error = %e, "failed to apply slice result, removing run");
                self.teardown_run(run_uid).await;
            }
        }
    }

    pub(super) async fn enqueue_next_dslice_from_run(&mut self, run_uid: &str) {
        if !self.run_manager.has_run(run_uid) {
            return;
        }
        let circuit_id = self
            .run_manager
            .get_circuit_id(run_uid)
            .map(|s| s.to_string());
        if let Some(cid) = circuit_id {
            if let Some(circuit) = self.circuit_store.get_circuit(&cid).cloned() {
                self.enqueue_next_dslice(run_uid, &circuit).await;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_failure(
        &mut self,
        uid: u16,
        request_type: RequestType,
        retry_count: u32,
        retry_payload: RetryPayload,
        run_uid: &Option<String>,
        slice_num: &Option<String>,
        is_tile: bool,
        _task_id: Option<&str>,
        tile_idx: Option<u32>,
        external_request_hash: Option<u32>,
        reason: &str,
    ) {
        warn!(uid = uid, rtype = %request_type, retry = retry_count, error = reason, "miner query failed");

        self.performance_tracker.record_reschedule(uid);

        let elapsed = 0.0;
        self.score_manager.update_score(
            uid,
            false,
            elapsed,
            VALIDATOR_REQUEST_TIMEOUT_SECONDS as f64,
            0.0,
            self.config.metagraph.n,
        );
        metrics::record_response(false, elapsed);

        let max_retries = match (&request_type, &retry_payload) {
            (RequestType::DSlice, RetryPayload::DSlice(ref d))
                if d.run_source == RunSource::Api =>
            {
                MAX_API_RETRIES
            }
            (RequestType::DSlice, _) => MAX_SLICE_RETRIES,
            _ => MAX_API_RETRIES,
        };

        let next_retry = retry_count + 1;

        if next_retry <= max_retries && self.attempt_retry(retry_payload, next_retry) {
            return;
        }

        if request_type == RequestType::DSlice {
            self.handle_dslice_max_retries(run_uid, slice_num, is_tile, tile_idx, reason)
                .await;
        }

        if let Some(req_id) = external_request_hash {
            let frame_type = match request_type {
                RequestType::Rwr | RequestType::ProofOfWeights => FRAME_PROOF_RESULT,
                _ => FRAME_SUBMIT_RESULT,
            };
            self.relay_send_response(
                frame_type,
                req_id,
                serde_json::json!({
                    "success": false,
                    "error": "max retries exceeded",
                }),
            )
            .await;
        }
    }
}
