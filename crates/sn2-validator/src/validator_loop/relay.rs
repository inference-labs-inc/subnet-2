use std::sync::Arc;

use tracing::warn;

use super::ValidatorLoop;
use crate::dsperse_events::DsperseEventClient;
use crate::relay::FRAME_SUBMIT_RESULT;
use crate::stats_reporter::{DsperseRunReport, DsperseSliceReport};

impl ValidatorLoop {
    pub(super) async fn relay_send_response(
        &self,
        msg_type: u8,
        req_id: u32,
        result: serde_json::Value,
    ) {
        if let Some(relay) = &self.relay {
            relay.send_response(msg_type, req_id, result).await;
        }
    }

    pub(super) async fn relay_send_notification(&self, method: &str, params: serde_json::Value) {
        if let Some(relay) = &self.relay {
            relay.send_notification(method, params).await;
        }
    }

    pub(super) async fn relay_register_pending(&self, hash: &str) {
        if let Some(relay) = &self.relay {
            relay.register_pending(hash).await;
        }
    }

    pub(super) async fn relay_remove_pending(&self, hash: &str) {
        if let Some(relay) = &self.relay {
            relay.remove_pending(hash).await;
        }
    }

    pub(super) async fn relay_set_request_result(
        &self,
        request_hash: &str,
        result: serde_json::Value,
    ) {
        if let Some(relay) = &self.relay {
            relay.set_request_result(request_hash, result).await;
        }
    }

    pub(super) fn emit_event<F, Fut>(&mut self, f: F)
    where
        F: FnOnce(Arc<DsperseEventClient>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        if let Some(ev) = &self.dsperse_events {
            let ev = Arc::clone(ev);
            self.dsperse_emit_tasks.spawn(async move {
                f(ev).await;
            });
        }
    }

    pub(super) async fn handle_rpc_disconnect(&mut self, context: &str) {
        warn!(context, "chain RPC disconnected, reconnecting");
        if let Err(re) = self.config.reconnect_chain_client().await {
            warn!(error = ?re, context, "chain reconnect failed");
        }
    }

    pub(super) async fn send_submit_error(&self, req_id: Option<u32>, error: &str) {
        if let Some(req_id) = req_id {
            self.relay_send_response(
                FRAME_SUBMIT_RESULT,
                req_id,
                serde_json::json!({"error": error}),
            )
            .await;
        }
    }

    pub(super) fn spawn_artifact_upload(
        &mut self,
        run_uid: &str,
        active_run: &mut Option<crate::incremental_runner::ActiveRun>,
        final_output: Option<serde_json::Value>,
    ) {
        let Some(uploader) = &self.proof_uploader else {
            return;
        };
        let artifacts = active_run
            .as_mut()
            .map(|r| std::mem::take(&mut r.artifacts))
            .unwrap_or_default();
        if artifacts.is_empty() {
            return;
        }
        let uploader = Arc::clone(uploader);
        let uid_clone = run_uid.to_string();
        let circuit_id = active_run
            .as_ref()
            .map(|r| r.circuit_id.clone())
            .unwrap_or_default();
        let circuit_name = active_run
            .as_ref()
            .map(|r| r.circuit_name.clone())
            .unwrap_or_default();
        self.upload_tasks.spawn(async move {
            if let Err(e) = uploader
                .upload_run_artifacts(
                    &uid_clone,
                    &circuit_id,
                    &circuit_name,
                    artifacts,
                    final_output,
                )
                .await
            {
                warn!(run_uid = %uid_clone, error = %e, "proof upload failed");
            }
        });
    }

    pub(super) async fn notify_run_completed(
        &mut self,
        run_uid: &str,
        active_run: &Option<crate::incremental_runner::ActiveRun>,
        final_output: Option<serde_json::Value>,
        failed_count: usize,
    ) {
        let notify_circuit_id = active_run
            .as_ref()
            .map(|r| r.circuit_id.as_str())
            .unwrap_or_default()
            .to_string();
        let status = if failed_count > 0 {
            "partial"
        } else {
            "complete"
        };
        let mut result = serde_json::json!({"run_uid": run_uid, "status": status});
        if let Some(output) = final_output {
            result["output"] = output;
        }
        if failed_count > 0 {
            result["failed_slices"] = serde_json::json!(failed_count);
        }
        self.relay_set_request_result(run_uid, result).await;
        let notification_status = if failed_count > 0 {
            "partial"
        } else {
            "completed"
        };
        let mut notification = serde_json::json!({
            "run_uid": run_uid,
            "circuit_id": notify_circuit_id,
            "status": notification_status,
        });
        if failed_count > 0 {
            notification["failed_slices"] = serde_json::json!(failed_count);
        }
        self.relay_send_notification("subnet-2.batch_completed", notification)
            .await;
    }

    pub(super) fn report_dsperse_completion(&self, run: &crate::incremental_runner::ActiveRun) {
        let reporter = match &self.stats_reporter {
            Some(r) => r,
            None => return,
        };

        let total_run_time_sec = run.started_at.elapsed().as_secs_f64();
        let mut failed_count = 0usize;
        let model_slices = run.combined.as_ref().map(|c| c.model_meta().slices.clone());
        let slice_reports: Vec<DsperseSliceReport> = run
            .artifacts
            .iter()
            .map(|a| {
                let success = a.proof_hex.is_some();
                if !success {
                    failed_count += 1;
                }
                let tiling = model_slices.as_ref().and_then(|slices| {
                    let idx = a
                        .slice_num
                        .strip_prefix("slice_")
                        .and_then(|s| s.parse::<usize>().ok())?;
                    slices
                        .iter()
                        .find(|s| s.index == idx)
                        .and_then(|s| s.tiling.as_ref())
                });
                DsperseSliceReport {
                    slice_num: a.slice_num.clone(),
                    proof_system: a
                        .proof_system
                        .map(|ps| ps.to_string())
                        .unwrap_or_else(|| "JSTPROVE".to_string()),
                    response_time_sec: a.response_time,
                    verification_time_sec: a.verification_time,
                    success,
                    is_tiled: tiling.is_some(),
                    tile_count: Some(tiling.map(|t| t.num_tiles).unwrap_or(1)),
                }
            })
            .collect();

        let all_successful = failed_count == 0 && !slice_reports.is_empty();

        let total_slices = run
            .combined
            .as_ref()
            .map(|c| c.model_meta().slices.len())
            .unwrap_or(slice_reports.len());

        reporter.report_dsperse_run(DsperseRunReport {
            run_uid: run.run_uid.clone(),
            circuit_id: run.circuit_id.clone(),
            circuit_name: run.circuit_name.clone(),
            total_slices,
            total_run_time_sec,
            all_successful,
            failed_slice_count: failed_count,
            slices: slice_reports,
        });
    }

    pub(super) fn spawn_emit_run_complete(
        &mut self,
        run: &crate::incremental_runner::ActiveRun,
        completed: bool,
    ) {
        let uid = run.run_uid.clone();
        let all_ok = completed
            && !run.artifacts.is_empty()
            && run.artifacts.iter().all(|a| a.proof_hex.is_some());
        let elapsed = run.started_at.elapsed().as_secs_f64();
        self.emit_event(move |ev| async move {
            ev.emit_run_complete(&uid, all_ok, elapsed).await;
        });
    }

    pub(super) async fn teardown_run(&mut self, run_uid: &str) {
        self.cleanup_run_resources(run_uid).await;
        let removed = self.run_manager.remove_run(run_uid);
        if let Some(ref run) = removed {
            self.spawn_emit_run_complete(run, false);
        }
        self.stacked_dslice_queue
            .retain(|req| req.run_uid != run_uid);
        self.api_dslice_queue.retain(|req| req.run_uid != run_uid);
        self.dslice_input_scales
            .retain(|(uid, _), _| uid != run_uid);
        self.relay_remove_pending(run_uid).await;
    }
}
