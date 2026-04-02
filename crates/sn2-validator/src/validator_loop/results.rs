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

        if let Some(outputs_json) = &response.computed_outputs {
            let miner_outputs: Vec<f64> = match serde_json::from_value(outputs_json.clone()) {
                Ok(v) => v,
                Err(e) => {
                    warn!(
                        uid = response.uid,
                        run_uid = %run_uid,
                        slice = %slice_num,
                        error = %e,
                        "malformed computed_outputs in miner response"
                    );
                    Vec::new()
                }
            };
            if !miner_outputs.is_empty() {
                use crate::incremental_runner::OutputConsistency;
                match self.run_manager.verify_output_consistency(
                    &run_uid,
                    &slice_num,
                    &miner_outputs,
                ) {
                    OutputConsistency::Consistent { max_rel_err } => {
                        tracing::debug!(
                            uid = response.uid,
                            run_uid = %run_uid,
                            slice = %slice_num,
                            max_rel_err,
                            "output consistency verified"
                        );
                    }
                    OutputConsistency::Diverged { max_rel_err } => {
                        warn!(
                            uid = response.uid,
                            run_uid = %run_uid,
                            slice = %slice_num,
                            max_rel_err,
                            "output consistency check failed: miner outputs diverge from expected"
                        );
                    }
                    OutputConsistency::LengthMismatch { expected, actual } => {
                        warn!(
                            uid = response.uid,
                            run_uid = %run_uid,
                            slice = %slice_num,
                            expected,
                            actual,
                            "output consistency check failed: length mismatch"
                        );
                    }
                    OutputConsistency::NoExpected | OutputConsistency::NoRun => {}
                }
            }
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

            use crate::incremental_runner::TileBufferOutcome;
            match self.run_manager.record_tile(&run_uid, &slice_num, tile_idx) {
                TileBufferOutcome::Waiting => return,
                TileBufferOutcome::AllReceived => {
                    self.run_manager.mark_slice_done(&run_uid, &slice_num);
                }
                TileBufferOutcome::Failed(reason) => {
                    warn!(
                        run_uid = %run_uid,
                        slice = %slice_num,
                        error = %reason,
                        "tile tracking failed, removing run"
                    );
                    self.teardown_run(&run_uid).await;
                    return;
                }
            }
        } else {
            self.run_manager.mark_slice_done(&run_uid, &slice_num);
        }

        if self.run_manager.is_run_complete(&run_uid) {
            self.finalize_combined_run(&run_uid).await;
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
