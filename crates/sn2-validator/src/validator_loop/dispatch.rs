use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use rand::seq::SliceRandom;
use sn2_types::*;

use super::{is_valid_ip, DispatchedRequest, RetryPayload, TaskOutcome, TaskResult, ValidatorLoop};
use crate::metrics_server as metrics;
use crate::relay::FRAME_PROOF_RESULT;

const DISPATCH_CACHE_TTL: Duration = Duration::from_millis(2000);

pub(crate) struct DispatchCache {
    pub capacities: HashMap<u16, usize>,
    pub adaptive_timeout: f64,
    pub api_eligible: HashSet<u16>,
    pub refreshed_at: Option<Instant>,
}

impl DispatchCache {
    pub fn new() -> Self {
        Self {
            capacities: HashMap::new(),
            adaptive_timeout: CIRCUIT_TIMEOUT_SECONDS as f64,
            api_eligible: HashSet::new(),
            refreshed_at: None,
        }
    }
}

impl ValidatorLoop {
    pub(super) async fn dispatch_requests(&mut self) -> Result<()> {
        let pending_cap = self.verification_concurrency.saturating_mul(4);
        if self.pending_verifications.len() >= pending_cap {
            return Ok(());
        }

        let active_count = self.tasks.len();
        let total_pipeline =
            active_count + self.verify_tasks.len() + self.pending_verifications.len();
        let dispatch_ceiling = self.verification_concurrency.saturating_mul(8);
        if total_pipeline >= dispatch_ceiling {
            return Ok(());
        }
        let mut dispatch_budget = dispatch_ceiling - total_pipeline;

        metrics::set_active_tasks(active_count);

        let mut queryable_uids: Vec<u16> = self
            .config
            .metagraph
            .neurons
            .iter()
            .filter(|n| {
                if let Some(targets) = &self.config.target_uids {
                    return targets.contains(&n.uid);
                }
                if n.validator_permit {
                    return false;
                }
                if n.axon_ip.is_empty() || n.axon_port == 0 {
                    return false;
                }
                is_valid_ip(&n.axon_ip)
            })
            .map(|n| n.uid)
            .collect();
        queryable_uids.shuffle(&mut rand::rng());

        self.refresh_dispatch_cache_if_stale(&queryable_uids);
        let adaptive_timeout = self.dispatch_cache.adaptive_timeout;

        for uid in queryable_uids {
            if dispatch_budget == 0 {
                break;
            }
            let cap = self
                .dispatch_cache
                .capacities
                .get(&uid)
                .copied()
                .unwrap_or(1);
            let active_now = self.miner_active_count.get(&uid).copied().unwrap_or(0);
            if active_now >= cap {
                continue;
            }
            let slots_for_miner = (cap - active_now).min(dispatch_budget);

            for _slot in 0..slots_for_miner {
                let active = self.miner_active_count.get(&uid).copied().unwrap_or(0);
                let was_at_capacity = active + 1 >= cap;

                let dispatched = match self.select_request(uid).await {
                    Some(d) => d,
                    None => break,
                };

                let timeout = if self.dispatch_cache.api_eligible.contains(&uid) {
                    API_TIMEOUT_SECONDS
                } else {
                    adaptive_timeout
                };

                let (ip, port, hotkey) = match self.config.metagraph.get_neuron(uid) {
                    Some(n) => (n.axon_ip.clone(), n.axon_port, n.hotkey.clone()),
                    None => break,
                };
                self.spawn_miner_task(uid, ip, port, hotkey, was_at_capacity, timeout, dispatched);
                dispatch_budget = dispatch_budget.saturating_sub(1);
            }
        }

        Ok(())
    }

    fn refresh_dispatch_cache_if_stale(&mut self, queryable_uids: &[u16]) {
        let fresh = self
            .dispatch_cache
            .refreshed_at
            .map(|t| t.elapsed() < DISPATCH_CACHE_TTL)
            .unwrap_or(false);
        if fresh {
            return;
        }
        self.dispatch_cache.capacities = self.performance_tracker.miner_capacities();
        self.dispatch_cache.adaptive_timeout = self.performance_tracker.adaptive_timeout();
        self.dispatch_cache.api_eligible = self.compute_api_eligible_from_uids(queryable_uids);
        self.dispatch_cache.refreshed_at = Some(Instant::now());
    }

    fn compute_api_eligible_from_uids(&self, queryable_uids: &[u16]) -> HashSet<u16> {
        if queryable_uids.is_empty() || self.config.api_miners_pct == 0 {
            return HashSet::new();
        }
        let snap = self.performance_tracker.snapshot();
        let queryable: HashSet<u16> = queryable_uids.iter().copied().collect();

        let mut ranked: Vec<(u16, f64)> = snap
            .iter()
            .filter(|(uid, (_, count))| {
                *count >= PERFORMANCE_MIN_SAMPLES && queryable.contains(uid)
            })
            .map(|(&uid, &(rate, _))| (uid, rate))
            .collect();

        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let top_count = (ranked.len() as u32 * self.config.api_miners_pct / 100).max(1) as usize;
        ranked
            .into_iter()
            .take(top_count)
            .map(|(uid, _)| uid)
            .collect()
    }

    async fn select_request(&mut self, uid: u16) -> Option<DispatchedRequest> {
        if let Some(rwr) = self.rwr_queue.pop_front() {
            let circuit = match self.circuit_store.ensure_circuit(&rwr.circuit_id).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(circuit = %rwr.circuit_id, error = %e, "unknown circuit for RWR");
                    if let Some(req_id) = rwr.request_id {
                        self.relay_send_response(
                            FRAME_PROOF_RESULT,
                            req_id,
                            serde_json::json!({"success": false, "error": format!("unknown circuit: {e}")}),
                        ).await;
                    }
                    return None;
                }
            };
            if let Err(msg) = circuit.validate_inputs(&rwr.inputs) {
                tracing::warn!(circuit = %rwr.circuit_id, error = %msg, "invalid inputs for RWR");
                if let Some(req_id) = rwr.request_id {
                    self.relay_send_response(
                        FRAME_PROOF_RESULT,
                        req_id,
                        serde_json::json!({"success": false, "error": format!("invalid input shape: {msg}")}),
                    )
                    .await;
                }
                return None;
            }
            let body = serde_json::json!({
                "model_id": circuit.id,
                "query_input": rwr.inputs,
            });
            let guard_hash = self.pipeline.check_hash(&body);
            if guard_hash.is_none() {
                self.rwr_queue.push_back(rwr);
                return None;
            }
            return Some(DispatchedRequest {
                request_type: RequestType::Rwr,
                guard_hash,
                external_request_hash: rwr.request_id,
                body,
                synapse_name: QueryZkProof::NAME,
                retry_count: rwr.retry_count,
                slice_num: None,
                run_uid: None,
                is_tile: false,
                task_id: None,
                tile_idx: None,
                task_circuit: Some(Arc::new(circuit.clone())),
                task_inputs: Some(rwr.inputs.clone()),
                task_proof_system: Some(circuit.proof_system),
                retry_payload: RetryPayload::Rwr(rwr),
                dsperse_circuit_path: None,
                component_sha: None,
            });
        }

        if let Some((dslice, queue_source)) = self
            .api_dslice_queue
            .pop_front()
            .map(|d| (d, RunSource::Api))
            .or_else(|| {
                self.stacked_dslice_queue
                    .pop_front()
                    .map(|d| (d, RunSource::Benchmark))
            })
        {
            let inputs_json = match sn2_types::decode_msgpack_to_json(&dslice.inputs) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        uid,
                        run_uid = %dslice.run_uid,
                        slice_num = %dslice.slice_num,
                        tile_idx = ?dslice.tile_idx,
                        error = %e,
                        "dropping dslice: failed to decode queued msgpack inputs"
                    );
                    return None;
                }
            };
            let dslice_model = self.pipeline.prepare_dslice_request(
                uid,
                &dslice.circuit,
                inputs_json.clone(),
                None,
                &dslice.slice_num,
                &dslice.run_uid,
                dslice.proof_system,
                dslice.component_sha.clone(),
            );
            let body = serde_json::to_value(&dslice_model).unwrap_or_default();
            let guard_hash = self.pipeline.check_dslice_hash(
                &dslice.circuit.id,
                &dslice.slice_num,
                &dslice.run_uid,
                dslice.tile_idx,
            );
            if guard_hash.is_none() {
                match queue_source {
                    RunSource::Api => self.api_dslice_queue.push_back(dslice),
                    RunSource::Benchmark => self.stacked_dslice_queue.push_back(dslice),
                }
                return None;
            }
            let circuit_path = dslice.circuit_path.clone();
            let component_sha = dslice.component_sha.clone();
            return Some(DispatchedRequest {
                request_type: RequestType::DSlice,
                guard_hash,
                external_request_hash: None,
                body,
                synapse_name: DSliceProofGenerationDataModel::NAME,
                retry_count: dslice.retry_count,
                slice_num: Some(dslice.slice_num.clone()),
                run_uid: Some(dslice.run_uid.clone()),
                is_tile: dslice.is_tile,
                task_id: dslice.task_id.clone(),
                tile_idx: dslice.tile_idx,
                task_circuit: Some(Arc::clone(&dslice.circuit)),
                task_inputs: Some(inputs_json),
                task_proof_system: Some(dslice.proof_system),
                retry_payload: RetryPayload::DSlice(Box::new(dslice)),
                dsperse_circuit_path: circuit_path,
                component_sha,
            });
        }

        None
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_miner_task(
        &mut self,
        uid: u16,
        ip: String,
        port: u16,
        hotkey: String,
        was_at_capacity: bool,
        timeout: f64,
        d: DispatchedRequest,
    ) {
        let client = Arc::clone(&self.miner_client);

        let request_type = d.request_type;
        let guard_hash = d.guard_hash;
        let external_request_hash = d.external_request_hash;
        let body = d.body;
        let synapse_name = d.synapse_name;
        let retry_count = d.retry_count;
        let slice_num = d.slice_num;
        let run_uid = d.run_uid;
        let is_tile = d.is_tile;
        let task_id = d.task_id;
        let tile_idx = d.tile_idx;
        let task_circuit = d.task_circuit;
        let task_inputs = d.task_inputs;
        let task_proof_system = d.task_proof_system;
        let retry_payload = d.retry_payload;
        let dsperse_circuit_path = d.dsperse_circuit_path;
        let dsperse_component_sha = d.component_sha;
        let task_guard_hash = guard_hash.clone();

        let abort_handle = self.tasks.spawn(async move {
            let tokio_task_id = tokio::task::id();

            let guard = client.read().await;
            let query_result = guard
                .query_miner(&ip, port, &hotkey, synapse_name, &body, timeout)
                .await;
            drop(guard);

            let outcome = match query_result {
                Ok((resp_body, elapsed)) => {
                    let mut response = MinerResponse {
                        uid,
                        verification_result: false,
                        external_request_hash: external_request_hash
                            .map(|id| id.to_string())
                            .unwrap_or_default(),
                        response_time: elapsed,
                        proof_size: 0,
                        circuit: task_circuit,
                        proof_system: task_proof_system,
                        verification_time: None,
                        proof_content: resp_body
                            .get("query_output")
                            .cloned()
                            .or_else(|| resp_body.get("proof").cloned()),
                        public_json: None,
                        inputs: task_inputs,
                        request_type: Some(request_type),
                        dsperse_slice_num: slice_num
                            .as_deref()
                            .and_then(|s| s.strip_prefix("slice_").unwrap_or(s).parse().ok()),
                        dsperse_run_uid: run_uid.clone(),
                        raw: Some(resp_body),
                        error: None,
                        save: false,
                        computed_outputs: None,
                        is_incremental: request_type == RequestType::DSlice,
                        witness: None,
                        dsperse_circuit_path,
                        component_sha: dsperse_component_sha,
                    };
                    response.proof_size = response
                        .proof_content
                        .as_ref()
                        .and_then(|v| v.as_str())
                        .map(|s| s.len())
                        .unwrap_or(0);

                    if let Some(raw) = &response.raw {
                        response.witness = raw
                            .get("witness")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                        response.computed_outputs = raw.get("computed_outputs").cloned();
                        if let Some(ps) = raw.get("public_signals") {
                            response.public_json = ps.as_array().map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            });
                        }
                    }
                    TaskOutcome::Success(Box::new(response))
                }
                Err(e) => TaskOutcome::Failure(format!("{e:#}")),
            };

            TaskResult {
                tokio_task_id,
                uid,
                request_type,
                guard_hash: guard_hash.clone(),
                external_request_hash,
                retry_count,
                was_at_capacity,
                slice_num,
                run_uid,
                is_tile,
                task_id,
                tile_idx,
                outcome,
                retry_payload,
            }
        });
        self.task_meta
            .insert(abort_handle.id(), (uid, task_guard_hash));

        *self.miner_active_count.entry(uid).or_insert(0) += 1;
        metrics::record_request_sent(&request_type.to_string());
    }

    pub(super) fn get_queryable_neurons(&self) -> Vec<&sn2_chain::NeuronInfo> {
        self.config
            .metagraph
            .neurons
            .iter()
            .filter(|n| {
                if let Some(targets) = &self.config.target_uids {
                    return targets.contains(&n.uid);
                }
                if n.validator_permit {
                    return false;
                }
                if n.axon_ip.is_empty() || n.axon_port == 0 {
                    return false;
                }
                is_valid_ip(&n.axon_ip)
            })
            .collect()
    }
}
