use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use sn2_types::*;

use super::verification::pre_decide_sample;
use super::{is_valid_ip, DispatchedRequest, RetryPayload, TaskOutcome, TaskResult, ValidatorLoop};
use crate::metrics_server as metrics;
use crate::relay::FRAME_PROOF_RESULT;

const DISPATCH_CACHE_TTL: Duration = Duration::from_millis(2000);

#[derive(Default)]
struct FairDispatchState {
    /// Monotonic service order keyed by hotkey. A missing hotkey is older than
    /// every miner that has already received work, which lets newly eligible
    /// miners enter the rotation without resetting established miners.
    last_served: HashMap<String, u64>,
    last_served_at: HashMap<String, Instant>,
    service_sequence: u64,
}

impl FairDispatchState {
    fn select_next(&self, candidates: &[DispatchCandidate]) -> Option<usize> {
        candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                candidate.available && candidate.projected_active < candidate.cap
            })
            .min_by(|(_, left), (_, right)| {
                left.projected_active
                    .cmp(&right.projected_active)
                    .then_with(|| {
                        self.last_served
                            .get(&left.hotkey)
                            .copied()
                            .unwrap_or(0)
                            .cmp(&self.last_served.get(&right.hotkey).copied().unwrap_or(0))
                    })
                    .then_with(|| left.hotkey.cmp(&right.hotkey))
            })
            .map(|(index, _)| index)
    }

    fn record_dispatch(&mut self, hotkey: &str) {
        self.service_sequence = self.service_sequence.saturating_add(1);
        self.last_served
            .insert(hotkey.to_string(), self.service_sequence);
        self.last_served_at
            .insert(hotkey.to_string(), Instant::now());
    }

    fn prune(&mut self, metagraph_hotkeys: &HashSet<String>) {
        self.last_served
            .retain(|hotkey, _| metagraph_hotkeys.contains(hotkey));
        self.last_served_at
            .retain(|hotkey, _| metagraph_hotkeys.contains(hotkey));
    }
}

struct DispatchCandidate {
    uid: u16,
    hotkey: String,
    ip: String,
    port: u16,
    cap: usize,
    initial_active: usize,
    projected_active: usize,
    dispatched: usize,
    available: bool,
}

impl DispatchCandidate {
    fn batch_reached_capacity(&self) -> bool {
        self.dispatched > 0 && self.projected_active >= self.cap
    }
}

fn effective_dispatch_ceiling(
    configured: Option<usize>,
    learned_capacity: usize,
    pressure_scale: f64,
) -> usize {
    let configured = configured.unwrap_or(usize::MAX);
    if learned_capacity == 0 || pressure_scale >= 1.0 {
        return configured;
    }

    let pressure_ceiling =
        ((learned_capacity as f64 * pressure_scale.clamp(0.0, 1.0)).floor() as usize).max(1);
    configured.min(pressure_ceiling)
}

pub(crate) struct DispatchCache {
    pub capacities: HashMap<u16, usize>,
    pub adaptive_timeout: f64,
    pub api_eligible: HashSet<u16>,
    pub authenticated: HashSet<String>,
    pub refreshed_at: Option<Instant>,
    fairness: FairDispatchState,
}

impl DispatchCache {
    pub fn new() -> Self {
        Self {
            capacities: HashMap::new(),
            adaptive_timeout: CIRCUIT_TIMEOUT_SECONDS as f64,
            api_eligible: HashSet::new(),
            authenticated: HashSet::new(),
            refreshed_at: None,
            fairness: FairDispatchState::default(),
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

        metrics::set_active_tasks(active_count);

        self.absorb_pending_evictions();
        self.refill_dslice_queues();

        let current_block = self.current_block;
        let queryable_miners: Vec<(u16, String, String, u16)> = self
            .config
            .metagraph
            .neurons
            .iter()
            .filter(|n| {
                if let Some(&until) = self.dispatch_cooldowns.get(&n.hotkey) {
                    if current_block < until {
                        return false;
                    }
                }
                if self.rsv.is_skiplisted(&n.hotkey, current_block) {
                    return false;
                }
                if !self.hotkey_reachable(&n.hotkey) {
                    return false;
                }
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
            .map(|n| (n.uid, n.hotkey.clone(), n.axon_ip.clone(), n.axon_port))
            .collect();

        let queryable_uids: Vec<u16> = queryable_miners.iter().map(|(uid, _, _, _)| *uid).collect();

        self.refresh_dispatch_cache_if_stale(&queryable_uids).await;
        let adaptive_timeout = self.dispatch_cache.adaptive_timeout;

        // Retain service history for temporarily unreachable/cooling-down
        // miners, but discard hotkeys that have actually left the metagraph.
        let metagraph_hotkeys: HashSet<String> = self
            .config
            .metagraph
            .neurons
            .iter()
            .map(|neuron| neuron.hotkey.clone())
            .collect();
        self.dispatch_cache.fairness.prune(&metagraph_hotkeys);

        let mut candidates: Vec<DispatchCandidate> = queryable_miners
            .into_iter()
            .map(|(uid, hotkey, ip, port)| {
                let cap = self
                    .dispatch_cache
                    .capacities
                    .get(&uid)
                    .copied()
                    .unwrap_or(1);
                let active = self.miner_active_count.get(&uid).copied().unwrap_or(0);
                DispatchCandidate {
                    uid,
                    hotkey,
                    ip,
                    port,
                    cap,
                    initial_active: active,
                    projected_active: active,
                    dispatched: 0,
                    available: true,
                }
            })
            .collect();

        let learned_capacity = candidates.iter().fold(0usize, |total, candidate| {
            total.saturating_add(candidate.cap)
        });
        let pressure_scale = self.dispatch_pressure.scale();
        let dispatch_ceiling = effective_dispatch_ceiling(
            self.config.dispatch_ceiling,
            learned_capacity,
            pressure_scale,
        );
        let queued_work =
            self.rwr_queue.len() + self.api_dslice_queue.len() + self.stacked_dslice_queue.len();
        if queued_work == 0 {
            return Ok(());
        }
        let dispatchable_headroom = candidates.iter().fold(0usize, |total, candidate| {
            total.saturating_add(candidate.cap.saturating_sub(candidate.projected_active))
        });

        if total_pipeline >= dispatch_ceiling {
            return Ok(());
        }
        let mut dispatch_budget = dispatch_ceiling - total_pipeline;
        let dispatch_opportunity = dispatch_budget.min(dispatchable_headroom).min(queued_work);
        let should_record_allocation = dispatch_opportunity > 0;
        let mut prepared_dispatches: Vec<(usize, DispatchedRequest)> = Vec::new();
        let mut fair_slots = 0usize;
        let mut residual_slots = 0usize;

        while dispatch_budget > 0 {
            if self.rwr_queue.is_empty()
                && self.api_dslice_queue.is_empty()
                && self.stacked_dslice_queue.is_empty()
            {
                break;
            }
            let Some(index) = self.dispatch_cache.fairness.select_next(&candidates) else {
                break;
            };
            let uid = candidates[index].uid;

            let mut dispatched = match self.select_request(uid).await {
                Some(dispatched) => dispatched,
                None => {
                    // A rejected/colliding queue item must not spin forever.
                    // Other miners still get one opportunity to consume any
                    // remaining valid work during this dispatch pass.
                    candidates[index].available = false;
                    continue;
                }
            };

            let hotkey = candidates[index].hotkey.clone();

            // Pre-decide the RSV sample before the miner task is prepared.
            // Force-verify paths (PoW, customer RWR, external-hash, API
            // dslice) bypass the roll; benchmark dslices take the random
            // 4% sample. For non-sampled benchmark dispatches we drop
            // task_inputs immediately so the validator's local input copy
            // is not retained for the full request lifetime.
            dispatched.pre_sampled = pre_decide_sample(
                &dispatched,
                &hotkey,
                self.current_block,
                self.blocks_per_tempo,
                &mut self.rsv,
            );
            if !dispatched.pre_sampled {
                dispatched.task_inputs = None;
            }

            let contenders = candidates
                .iter()
                .filter(|candidate| {
                    candidate.available && candidate.projected_active < candidate.cap
                })
                .count();
            if contenders > 1 {
                fair_slots += 1;
            } else {
                residual_slots += 1;
            }
            candidates[index].projected_active += 1;
            candidates[index].dispatched += 1;
            self.dispatch_cache.fairness.record_dispatch(&hotkey);
            prepared_dispatches.push((index, dispatched));
            dispatch_budget -= 1;
        }

        // Saturation is a property of the completed allocation batch, not
        // only of the final request that happened to fill a miner's cap. Mark
        // every request in a batch that reaches the cap so completion-rate
        // learning observes the whole offered load even when several task
        // completions were drained before this dispatch call ran.
        for (index, dispatched) in prepared_dispatches {
            let candidate = &candidates[index];
            let uid = candidate.uid;
            let ip = candidate.ip.clone();
            let port = candidate.port;
            let hotkey = candidate.hotkey.clone();
            let was_at_capacity = candidate.batch_reached_capacity();
            let timeout = if self.dispatch_cache.api_eligible.contains(&uid) {
                API_TIMEOUT_SECONDS
            } else {
                adaptive_timeout
            };
            self.spawn_miner_task(uid, ip, port, hotkey, was_at_capacity, timeout, dispatched);
        }

        if should_record_allocation {
            let dispatched = candidates
                .iter()
                .map(|candidate| candidate.dispatched)
                .sum::<usize>();
            let starved_miners = if dispatch_budget == 0 {
                candidates
                    .iter()
                    .filter(|candidate| {
                        candidate.initial_active == 0
                            && candidate.dispatched == 0
                            && candidate.cap > 0
                    })
                    .count()
            } else {
                0
            };
            let utilization = dispatched as f64 / dispatch_opportunity as f64;
            metrics::record_dispatch_allocation(
                fair_slots,
                residual_slots,
                starved_miners,
                utilization,
            );

            if tracing::enabled!(tracing::Level::DEBUG) {
                for candidate in &candidates {
                    let last_dispatch_age_secs = self
                        .dispatch_cache
                        .fairness
                        .last_served_at
                        .get(&candidate.hotkey)
                        .map(|at| at.elapsed().as_secs_f64());
                    tracing::debug!(
                        uid = candidate.uid,
                        learned_cap = candidate.cap,
                        effective_cap = candidate.cap.min(dispatch_ceiling),
                        active_depth = candidate.projected_active,
                        dispatched = candidate.dispatched,
                        last_dispatch_age_secs = ?last_dispatch_age_secs,
                        pressure_scale,
                        "fair dispatch allocation"
                    );
                }
            }
        }

        Ok(())
    }

    fn absorb_pending_evictions(&mut self) {
        let evicted = self.performance_tracker.drain_pending_evictions();
        if evicted.is_empty() {
            return;
        }
        let until = self.current_block.saturating_add(sn2_types::REHAB_BLOCKS);
        for (uid, hotkey) in &evicted {
            let prev = self.dispatch_cooldowns.get(hotkey).copied().unwrap_or(0);
            let new_until = std::cmp::max(prev, until);
            self.dispatch_cooldowns.insert(hotkey.clone(), new_until);
            if prev == 0 {
                tracing::info!(
                    uid = *uid,
                    until_block = new_until,
                    rehab_blocks = sn2_types::REHAB_BLOCKS,
                    "miner evicted from dispatch"
                );
            }
        }
        self.dispatch_cache.refreshed_at = None;
    }

    /// Whether a hotkey holds an authenticated connection binding, so dispatch
    /// can actually route to it. Permissive while the cached set is empty
    /// (startup / transient) so selection never stalls; routing itself is the
    /// authoritative selector.
    pub(super) fn hotkey_reachable(&self, hotkey: &str) -> bool {
        let authenticated = &self.dispatch_cache.authenticated;
        authenticated.is_empty() || authenticated.contains(hotkey)
    }

    async fn refresh_dispatch_cache_if_stale(&mut self, queryable_uids: &[u16]) {
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
        self.dispatch_cache.authenticated = {
            let guard = self.miner_client.read().await;
            guard.authenticated_hotkeys().await
        };
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
                pre_sampled: false,
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
                pre_sampled: false,
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
        let pre_sampled = d.pre_sampled;
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

                    if let Some(raw) = response.raw.take() {
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
                pre_sampled,
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
                if !self.hotkey_reachable(&n.hotkey) {
                    return false;
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(hotkey: &str, cap: usize, projected_active: usize) -> DispatchCandidate {
        DispatchCandidate {
            uid: 0,
            hotkey: hotkey.to_string(),
            ip: String::new(),
            port: 0,
            cap,
            initial_active: projected_active,
            projected_active,
            dispatched: 0,
            available: true,
        }
    }

    fn allocate(
        state: &mut FairDispatchState,
        candidates: &mut [DispatchCandidate],
        budget: usize,
    ) -> HashMap<String, usize> {
        let mut allocations = HashMap::new();
        for _ in 0..budget {
            let Some(index) = state.select_next(candidates) else {
                break;
            };
            candidates[index].projected_active += 1;
            candidates[index].dispatched += 1;
            let hotkey = candidates[index].hotkey.clone();
            state.record_dispatch(&hotkey);
            *allocations.entry(hotkey).or_insert(0) += 1;
        }
        allocations
    }

    #[test]
    fn progressive_allocation_fills_small_caps_then_sends_residual_to_large_cap() {
        let mut state = FairDispatchState::default();
        let mut candidates = vec![
            candidate("top", 300, 0),
            candidate("new-eight", 8, 0),
            candidate("new-nine", 9, 0),
            candidate("cold", 1, 0),
        ];

        let allocations = allocate(&mut state, &mut candidates, 100);

        assert_eq!(allocations.get("cold"), Some(&1));
        assert_eq!(allocations.get("new-eight"), Some(&8));
        assert_eq!(allocations.get("new-nine"), Some(&9));
        assert_eq!(allocations.get("top"), Some(&82));
    }

    #[test]
    fn persistent_service_order_rotates_when_budget_is_smaller_than_miner_count() {
        let mut state = FairDispatchState::default();
        let miners = ["a", "b", "c", "d"];

        let mut first_pass: Vec<_> = miners
            .iter()
            .map(|hotkey| candidate(hotkey, 10, 0))
            .collect();
        let first = allocate(&mut state, &mut first_pass, 2);
        assert_eq!(
            first.keys().cloned().collect::<HashSet<_>>(),
            HashSet::from(["a".to_string(), "b".to_string(),])
        );

        // Model both first-pass requests completing before the next dispatch
        // call. The persistent sequence must favor the miners not yet served.
        let mut second_pass: Vec<_> = miners
            .iter()
            .map(|hotkey| candidate(hotkey, 10, 0))
            .collect();
        let second = allocate(&mut state, &mut second_pass, 2);
        assert_eq!(
            second.keys().cloned().collect::<HashSet<_>>(),
            HashSet::from(["c".to_string(), "d".to_string(),])
        );
    }

    #[test]
    fn projected_depth_precedes_service_age() {
        let mut state = FairDispatchState::default();
        state.record_dispatch("shallow");
        let candidates = vec![
            candidate("old-but-deep", 10, 5),
            candidate("shallow", 10, 2),
        ];

        let selected = state.select_next(&candidates).unwrap();

        assert_eq!(candidates[selected].hotkey, "shallow");
    }

    #[test]
    fn service_history_is_only_pruned_for_removed_hotkeys() {
        let mut state = FairDispatchState::default();
        state.record_dispatch("still-registered");
        state.record_dispatch("removed");

        state.prune(&HashSet::from(["still-registered".to_string()]));

        assert!(state.last_served.contains_key("still-registered"));
        assert!(!state.last_served.contains_key("removed"));
        assert!(state.last_served_at.contains_key("still-registered"));
        assert!(!state.last_served_at.contains_key("removed"));
    }

    #[test]
    fn every_request_in_a_full_refill_batch_is_saturated() {
        let mut state = FairDispatchState::default();
        let mut full = vec![candidate("full", 10, 5)];
        let allocations = allocate(&mut state, &mut full, 5);
        assert_eq!(allocations.get("full"), Some(&5));
        assert!(full[0].batch_reached_capacity());

        let mut partial = vec![candidate("partial", 10, 5)];
        let allocations = allocate(&mut state, &mut partial, 4);
        assert_eq!(allocations.get("partial"), Some(&4));
        assert!(!partial[0].batch_reached_capacity());
    }

    #[test]
    fn pressure_ceiling_is_additional_and_inactive_at_full_scale() {
        assert_eq!(effective_dispatch_ceiling(None, 400, 1.0), usize::MAX);
        assert_eq!(effective_dispatch_ceiling(None, 400, 0.9), 360);
        assert_eq!(effective_dispatch_ceiling(Some(200), 400, 0.9), 200);
        assert_eq!(effective_dispatch_ceiling(Some(500), 400, 0.9), 360);
        assert_eq!(effective_dispatch_ceiling(Some(0), 400, 0.9), 0);
        assert_eq!(effective_dispatch_ceiling(Some(500), 0, 0.9), 500);
        assert_eq!(effective_dispatch_ceiling(None, 1, 0.01), 1);
    }
}
