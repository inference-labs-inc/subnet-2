use sn2_types::*;
use tracing::info;

use super::{event_slice_num, RetryPayload, TaskOutcome, TaskResult, ValidatorLoop, VerifyResult};
use crate::relay::FRAME_PROOF_RESULT;
use crate::response_processor::ResponseProcessor;

impl ValidatorLoop {
    pub(super) async fn start_verification(&mut self, mut result: TaskResult) {
        let uid = result.uid;

        if let Some(count) = self.miner_active_count.get_mut(&uid) {
            *count = count.saturating_sub(1);
        }

        let sample = decide_sample(&result);
        if !sample && !response_needs_proof_bytes_for_downstream(&result) {
            if let TaskOutcome::Success(ref mut response) = result.outcome {
                response.proof_content = None;
                response.computed_outputs = None;
                response.witness = None;
                response.raw = None;
                response.public_json = None;
                response.inputs = None;
            }
        }

        let needs_real_verify =
            sample && matches!(result.outcome, TaskOutcome::Success(ref r) if r.proof_size > 0);
        if !needs_real_verify {
            let guard_hash = result.guard_hash.clone();
            let hotkey = self.uid_hotkeys.get(&uid).cloned().unwrap_or_default();
            let verified =
                matches!(result.outcome, TaskOutcome::Success(ref r) if r.proof_size > 0);
            if verified {
                if let TaskOutcome::Success(ref mut response) = result.outcome {
                    response.verification_result = true;
                }
            }
            let vr = VerifyResult {
                verify_task_id: None,
                task_result: result,
                verified,
                hotkey,
            };
            self.finish_verification(vr, guard_hash).await;
            return;
        }

        if self.verify_tasks.len() >= self.verification_concurrency {
            self.pending_verifications.push_back((result, sample));
            return;
        }
        self.spawn_verification(result, sample);
    }

    pub(super) fn drain_pending_verifications(&mut self) {
        while self.verify_tasks.len() < self.verification_concurrency {
            match self.pending_verifications.pop_front() {
                Some((result, sample)) => self.spawn_verification(result, sample),
                None => break,
            }
        }
    }

    fn spawn_verification(&mut self, mut result: TaskResult, sample: bool) {
        let guard_hash = result.guard_hash.clone();
        let uid = result.uid;
        let hotkey = self.uid_hotkeys.get(&uid).cloned().unwrap_or_default();
        // proof_size is set at response construction in dispatch and remains
        // valid even after the byte-shedding in start_verification cleared
        // proof_content for the no-op verify path. Gate on that instead of
        // the bytes themselves.
        let handle = match result.outcome {
            TaskOutcome::Success(ref mut response) if response.proof_size > 0 => {
                let mut response = match std::mem::replace(
                    &mut result.outcome,
                    TaskOutcome::Failure(String::new()),
                ) {
                    TaskOutcome::Success(r) => r,
                    _ => unreachable!(),
                };
                if !sample {
                    response.verification_result = true;
                    result.outcome = TaskOutcome::Success(response);
                    let captured_hotkey = hotkey.clone();
                    self.verify_tasks.spawn(async move {
                        VerifyResult {
                            verify_task_id: Some(tokio::task::id()),
                            task_result: result,
                            verified: true,
                            hotkey: captured_hotkey,
                        }
                    })
                } else {
                    let processor = ResponseProcessor::new();
                    let captured_hotkey = hotkey.clone();
                    self.verify_tasks.spawn(async move {
                        let verify_task_id = Some(tokio::task::id());
                        let verified =
                            matches!(processor.verify_response(&mut response).await, Ok(true));
                        response.verification_result = verified;
                        result.outcome = TaskOutcome::Success(response);
                        VerifyResult {
                            verify_task_id,
                            task_result: result,
                            verified,
                            hotkey: captured_hotkey,
                        }
                    })
                }
            }
            _ => {
                let captured_hotkey = hotkey.clone();
                self.verify_tasks.spawn(async move {
                    VerifyResult {
                        verify_task_id: Some(tokio::task::id()),
                        task_result: result,
                        verified: false,
                        hotkey: captured_hotkey,
                    }
                })
            }
        };
        self.verify_guard_hashes.insert(handle.id(), guard_hash);
    }

    pub(super) async fn finish_verification(
        &mut self,
        vr: VerifyResult,
        guard_hash: Option<String>,
    ) {
        let result = vr.task_result;
        let verified = vr.verified;
        let uid = result.uid;
        let dispatch_hotkey = vr.hotkey;
        if !dispatch_hotkey.is_empty() {
            self.rsv.observe(&dispatch_hotkey, self.current_block);
        }
        let was_at_capacity = result.was_at_capacity;
        let request_type = result.request_type;
        let run_uid = result.run_uid.clone();
        let slice_num = result.slice_num.clone();
        let is_tile = result.is_tile;
        let task_id = result.task_id.clone();
        let tile_idx = result.tile_idx;
        let external_request_hash = result.external_request_hash;
        let retry_count = result.retry_count;
        let mut result = result;
        let retry_payload = std::mem::replace(&mut result.retry_payload, RetryPayload::None);

        let failed = match result.outcome {
            TaskOutcome::Success(ref response) => {
                if verified {
                    if request_type == RequestType::ProofOfWeights {
                        self.handle_pow_success(response).await;
                        info!(uid = uid, rtype = %request_type, "PoW proof verified, scores applied");
                        None
                    } else {
                        let elapsed = response.response_time;
                        let verification_time = response.verification_time.unwrap_or(0.0);

                        self.record_verified_score(uid, response, was_at_capacity);

                        info!(uid = uid, elapsed = format!("{elapsed:.3}s"), rtype = %request_type, "proof verified");

                        if request_type == RequestType::DSlice {
                            if let (Some(ref ruid), Some(ref snum)) = (&run_uid, &slice_num) {
                                let ruid = ruid.clone();
                                let event_snum = event_slice_num(snum, is_tile, tile_idx);
                                self.emit_event(move |ev| async move {
                                    ev.emit_proof_received(&ruid, &event_snum, elapsed, uid)
                                        .await;
                                    ev.emit_verification_complete(
                                        &ruid,
                                        &event_snum,
                                        verification_time,
                                        true,
                                    )
                                    .await;
                                });
                            }

                            self.handle_dslice_success(
                                &run_uid,
                                &slice_num,
                                is_tile,
                                task_id.as_deref(),
                                tile_idx,
                                response,
                                verification_time,
                            )
                            .await;
                        }

                        if let Some(req_id) = external_request_hash {
                            self.relay_send_response(
                                FRAME_PROOF_RESULT,
                                req_id,
                                serde_json::json!({
                                    "success": true,
                                    "proof": response.proof_content,
                                    "public_signals": response.public_json,
                                }),
                            )
                            .await;
                        }
                        None
                    }
                } else {
                    if request_type == RequestType::DSlice {
                        if let (Some(ref ruid), Some(ref snum)) = (&run_uid, &slice_num) {
                            let ruid = ruid.clone();
                            let event_snum = event_slice_num(snum, is_tile, tile_idx);
                            let vt = response.verification_time.unwrap_or(0.0);
                            self.emit_event(move |ev| async move {
                                ev.emit_verification_complete(&ruid, &event_snum, vt, false)
                                    .await;
                            });
                        }
                    }
                    Some("verification failed".to_string())
                }
            }
            TaskOutcome::Failure(ref e) => Some(e.clone()),
        };

        if let TaskOutcome::Success(ref response) = result.outcome {
            if let Some(reporter) = &mut self.stats_reporter {
                reporter.record_response(response.as_ref(), &self.uid_hotkeys);
            }
        }

        if let Some(reason) = failed {
            self.handle_failure(
                uid,
                &dispatch_hotkey,
                request_type,
                retry_count,
                retry_payload,
                &run_uid,
                &slice_num,
                is_tile,
                task_id.as_deref(),
                tile_idx,
                external_request_hash,
                &reason,
            )
            .await;
        }

        if let Some(hash) = &guard_hash {
            if !hash.is_empty() {
                self.pipeline.release_hash(hash);
            }
        }
    }
}

fn decide_sample(result: &TaskResult) -> bool {
    let has_proof = matches!(result.outcome, TaskOutcome::Success(ref r) if r.proof_size > 0);
    if !has_proof {
        return false;
    }
    // The pre-decision attached at dispatch is authoritative. The historical
    // empty-hotkey force-sample branch was a safety net for the RSV roll
    // that used to run here, but it has migrated to pre_decide_sample at
    // dispatch time. Re-checking hotkey state at verify time would let a
    // post-dispatch metagraph reshuffle (uid deregistered between dispatch
    // and response) revive sampling on a request whose task_inputs were
    // already released, leading to a verification attempt against a missing
    // input tensor. pre_sampled alone is the correct signal here.
    result.pre_sampled
}

/// Dispatch-time mirror of decide_sample. Runs before the miner task is
/// spawned so the validator can drop its local copy of task_inputs (and any
/// other per-request state that would only be needed for deep verification)
/// for the ~96% of dispatches that take the RSV fast path. Empty hotkey is
/// preserved as a force-verify case to keep parity with the response-side
/// decision in case the metagraph is mid-sync at dispatch time.
pub(super) fn pre_decide_sample(
    dispatched: &super::DispatchedRequest,
    hotkey: &str,
    current_block: u64,
    blocks_per_tempo: u64,
    rsv: &mut crate::rsv::RsvManager,
) -> bool {
    let is_pow = dispatched.request_type == RequestType::ProofOfWeights;
    let is_customer_rwr = dispatched.request_type == RequestType::Rwr;
    let has_external_hash = dispatched.external_request_hash.is_some();
    let is_api_dslice = matches!(
        &dispatched.retry_payload,
        RetryPayload::DSlice(d) if d.run_source == RunSource::Api
    );
    let force_verify = is_pow || is_customer_rwr || has_external_hash || is_api_dslice;
    force_verify || hotkey.is_empty() || rsv.should_sample(hotkey, current_block, blocks_per_tempo)
}

/// True when a successful response's proof bytes are consumed by something
/// other than the verifier — either relayed back to a customer or pushed
/// into run_manager.artifacts for upload. False for benchmark dslices and
/// other purely-internal flows where the bytes have no downstream reader.
fn response_needs_proof_bytes_for_downstream(result: &TaskResult) -> bool {
    if result.external_request_hash.is_some() {
        return true;
    }
    matches!(
        &result.retry_payload,
        RetryPayload::DSlice(d) if d.run_source == RunSource::Api
    )
}
