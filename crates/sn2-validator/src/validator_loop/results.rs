use sn2_types::*;
use tracing::{debug, info, warn};

use super::{event_slice_num, RetryPayload, ValidatorLoop};
use crate::incremental_runner::SliceArtifact;
use crate::metrics_server as metrics;
use crate::relay::{FRAME_PROOF_RESULT, FRAME_SUBMIT_RESULT};

impl ValidatorLoop {
    fn build_work_key(circuit_id: Option<&str>, slice: Option<&str>) -> String {
        match (circuit_id, slice) {
            (Some(id), Some(slice)) => format!("{id}:{slice}"),
            (Some(id), None) => id.to_string(),
            (None, Some(slice)) => format!("slice:{slice}"),
            (None, None) => String::new(),
        }
    }

    pub(super) fn record_verified_score(
        &mut self,
        uid: u16,
        response: &MinerResponse,
        was_at_capacity: bool,
    ) {
        let elapsed = response.response_time;
        let hotkey = self.uid_hotkeys.get(&uid).cloned().unwrap_or_default();
        let circuit_id = response.circuit.as_ref().map(|c| c.id.as_str());
        let slice = response.dsperse_slice_num.map(|n| n.to_string());
        let work_key = Self::build_work_key(circuit_id, slice.as_deref());
        self.performance_tracker.record_keyed(
            uid,
            &hotkey,
            true,
            elapsed,
            was_at_capacity,
            &work_key,
        );
        self.score_manager.update_score(
            uid,
            true,
            elapsed,
            VALIDATOR_REQUEST_TIMEOUT_SECONDS as f64,
            0.0,
            self.config.metagraph.n,
        );
        metrics::record_response(true, elapsed);
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
            if !self.run_manager.has_run(run_uid) {
                return;
            }

            if let Some(snum) = slice_num {
                if !self.run_manager.is_slice_failed(run_uid, snum) {
                    let ruid = run_uid.clone();
                    let event_snum = event_slice_num(snum, is_tile, tile_idx);
                    let err = reason.to_string();
                    self.emit_event(move |ev| async move {
                        ev.emit_slice_failed(&ruid, &event_snum, &err).await;
                    });

                    let failed_count = self.run_manager.mark_slice_failed(run_uid, snum);
                    warn!(
                        run_uid = %run_uid,
                        slice = %snum,
                        failed_count,
                        "slice max retries exceeded, continuing run"
                    );
                }
            }

            if self.run_manager.is_run_complete(run_uid) {
                self.finalize_combined_run(run_uid).await;
            }
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

        if self.run_manager.is_slice_failed(&run_uid, &slice_num) {
            debug!(
                run_uid = %run_uid,
                slice = %slice_num,
                "ignoring late success for failed slice"
            );
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
                let circuit_output_names = response
                    .dsperse_circuit_path
                    .as_deref()
                    .and_then(|path| {
                        let backend = dsperse::backend::jstprove::JstproveBackend::new();
                        backend
                            .load_params(std::path::Path::new(path))
                            .ok()
                            .flatten()
                    })
                    .map(|params| {
                        params
                            .outputs
                            .iter()
                            .map(|o| o.name.clone())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                if circuit_output_names.is_empty() {
                    tracing::debug!(
                        uid = response.uid,
                        slice = %slice_num,
                        "skipping output consistency: no circuit output names"
                    );
                } else {
                    let norm_factor = self
                        .dslice_input_scales
                        .get(&(run_uid.clone(), slice_num.clone()))
                        .copied();
                    match self.run_manager.verify_output_consistency(
                        &run_uid,
                        &miner_outputs,
                        norm_factor,
                        &circuit_output_names,
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
                            let zk_sample: Vec<f64> =
                                miner_outputs.iter().copied().take(5).collect();
                            warn!(
                                uid = response.uid,
                                run_uid = %run_uid,
                                slice = %slice_num,
                                max_rel_err,
                                norm_factor = ?norm_factor,
                                num_outputs = circuit_output_names.len(),
                                zk_len = miner_outputs.len(),
                                zk_sample = ?zk_sample,
                                "output consistency check failed: miner outputs diverge"
                            );
                        }
                        OutputConsistency::LengthMismatch { expected, actual } => {
                            warn!(
                                uid = response.uid,
                                run_uid = %run_uid,
                                slice = %slice_num,
                                expected,
                                actual,
                                "output consistency check failed: empty outputs"
                            );
                        }
                        OutputConsistency::NoExpected | OutputConsistency::NoRun => {}
                    }
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
        hotkey: &str,
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
        warn!(uid = uid, rtype = %request_type, retry = retry_count, run_uid = ?run_uid, slice = ?slice_num, tile = ?tile_idx, error = reason, "miner query failed");

        if !hotkey.is_empty()
            && (reason.contains("already in progress") || reason.contains("in backoff"))
        {
            let bpt = if self.blocks_per_tempo == 0 {
                360
            } else {
                self.blocks_per_tempo
            };
            let until = self.current_block + bpt;
            self.dispatch_cooldowns.insert(hotkey.to_string(), until);
        }

        let is_verification_failure = reason.starts_with("verification failed")
            && matches!(
                request_type,
                RequestType::ProofOfWeights | RequestType::Rwr | RequestType::DSlice
            );
        if is_verification_failure && !hotkey.is_empty() {
            let triggered =
                self.rsv
                    .record_strike(hotkey, self.current_block, self.blocks_per_tempo);
            if triggered {
                info!(uid, "rsv: skiplist triggered via failure path");
            }
        }

        let failed_circuit_id = match &retry_payload {
            RetryPayload::DSlice(d) => Some(d.circuit.id.as_str()),
            RetryPayload::Rwr(r) => Some(r.circuit_id.as_str()),
            RetryPayload::None => None,
        };
        let slice_part = slice_num
            .as_deref()
            .map(|s| s.strip_prefix("slice_").unwrap_or(s));
        let work_key = Self::build_work_key(failed_circuit_id, slice_part);
        self.performance_tracker
            .record_reschedule_keyed(uid, &work_key);

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

        if !is_verification_failure
            && next_retry <= max_retries
            && self.attempt_retry(retry_payload, next_retry)
        {
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
