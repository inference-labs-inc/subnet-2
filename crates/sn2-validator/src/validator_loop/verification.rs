use sn2_types::*;
use tracing::info;

use super::{event_slice_num, RetryPayload, TaskOutcome, TaskResult, ValidatorLoop, VerifyResult};
use crate::relay::FRAME_PROOF_RESULT;
use crate::response_processor::ResponseProcessor;

impl ValidatorLoop {
    pub(super) fn start_verification(&mut self, result: TaskResult) {
        let uid = result.uid;

        if let Some(count) = self.miner_active_count.get_mut(&uid) {
            *count = count.saturating_sub(1);
        }
        if result.request_type == RequestType::Benchmark {
            self.benchmark_in_flight = self.benchmark_in_flight.saturating_sub(1);
        }

        if self.verify_tasks.len() >= self.verification_concurrency {
            self.pending_verifications.push_back(result);
            return;
        }

        self.spawn_verification(result);
    }

    pub(super) fn drain_pending_verifications(&mut self) {
        while self.verify_tasks.len() < self.verification_concurrency {
            match self.pending_verifications.pop_front() {
                Some(result) => self.spawn_verification(result),
                None => break,
            }
        }
    }

    fn spawn_verification(&mut self, mut result: TaskResult) {
        let guard_hash = result.guard_hash.clone();
        let handle = match result.outcome {
            TaskOutcome::Success(ref mut response) if response.proof_content.is_some() => {
                let mut response = match std::mem::replace(
                    &mut result.outcome,
                    TaskOutcome::Failure(String::new()),
                ) {
                    TaskOutcome::Success(r) => r,
                    _ => unreachable!(),
                };
                let processor = ResponseProcessor::new();
                self.verify_tasks.spawn(async move {
                    let verify_task_id = tokio::task::id();
                    let verified =
                        matches!(processor.verify_response(&mut response).await, Ok(true));
                    response.verification_result = verified;
                    result.outcome = TaskOutcome::Success(response);
                    VerifyResult {
                        verify_task_id,
                        task_result: result,
                        verified,
                    }
                })
            }
            _ => self.verify_tasks.spawn(async move {
                VerifyResult {
                    verify_task_id: tokio::task::id(),
                    task_result: result,
                    verified: false,
                }
            }),
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
