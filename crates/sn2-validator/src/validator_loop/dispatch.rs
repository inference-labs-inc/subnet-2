use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use sn2_types::*;

use super::{is_valid_ip, DispatchedRequest, RetryPayload, TaskOutcome, TaskResult, ValidatorLoop};
use crate::metrics_server as metrics;
use crate::pow_manager::PowManager;
use crate::relay::FRAME_PROOF_RESULT;

impl ValidatorLoop {
    pub(super) async fn dispatch_requests(&mut self) -> Result<()> {
        let verification_backlog = self.verify_tasks.len() + self.pending_verifications.len();
        if verification_backlog >= self.verification_concurrency {
            return Ok(());
        }

        let active_count = self.tasks.len();
        let total_pipeline = active_count + verification_backlog;
        let dispatch_ceiling = self.verification_concurrency.saturating_mul(2);
        if total_pipeline >= dispatch_ceiling {
            return Ok(());
        }
        let mut dispatch_budget = dispatch_ceiling - total_pipeline;

        metrics::set_active_tasks(active_count);

        let capacities = self.performance_tracker.miner_capacities();
        let adaptive_timeout = self.performance_tracker.adaptive_timeout();

        let queryable = self.get_queryable_neurons();
        let mut neurons: Vec<sn2_chain::NeuronInfo> = queryable.into_iter().cloned().collect();
        rand::seq::SliceRandom::shuffle(neurons.as_mut_slice(), &mut rand::thread_rng());

        let neuron_refs: Vec<&sn2_chain::NeuronInfo> = neurons.iter().collect();
        let api_eligible = self.compute_api_eligible(&neuron_refs);
        let benchmark_circuits = self.circuit_store.get_benchmark_circuits();

        let pow_ready = self.pow_manager.should_batch();
        let pow_circuit = if pow_ready {
            self.circuit_store
                .ensure_circuit(BATCHED_PROOF_OF_WEIGHTS_MODEL_ID)
                .await
                .ok()
        } else {
            None
        };

        for neuron in &neurons {
            if dispatch_budget == 0 {
                break;
            }
            let uid = neuron.uid;
            let cap = capacities.get(&uid).copied().unwrap_or(1);
            let active_now = self.miner_active_count.get(&uid).copied().unwrap_or(0);
            if active_now >= cap {
                continue;
            }
            let slots_for_miner = (cap - active_now).min(dispatch_budget);

            for _slot in 0..slots_for_miner {
                let active = self.miner_active_count.get(&uid).copied().unwrap_or(0);
                let was_at_capacity = active + 1 >= cap;

                let dispatched = match self
                    .select_request(uid, &pow_circuit, &benchmark_circuits)
                    .await
                {
                    Some(d) => d,
                    None => break,
                };

                let timeout = if api_eligible.contains(&uid) {
                    API_TIMEOUT_SECONDS
                } else {
                    adaptive_timeout
                };

                self.spawn_miner_task(uid, neuron, was_at_capacity, timeout, dispatched);
                dispatch_budget = dispatch_budget.saturating_sub(1);
            }
        }

        Ok(())
    }

    async fn select_request(
        &mut self,
        uid: u16,
        pow_circuit: &Option<Circuit>,
        benchmark_circuits: &[Circuit],
    ) -> Option<DispatchedRequest> {
        if let Some(pow_circ) = pow_circuit
            .as_ref()
            .filter(|_| self.pow_manager.should_batch())
        {
            let items = self.pow_manager.drain_batch();
            let inputs = PowManager::prepare_inputs(&items);
            let body = serde_json::json!({
                "subnet_uid": self.config.netuid,
                "verification_key_hash": pow_circ.id,
                "proof_system": pow_circ.proof_system.to_string(),
                "inputs": inputs,
                "proof": "",
                "public_signals": "",
            });
            return Some(DispatchedRequest {
                request_type: RequestType::ProofOfWeights,
                guard_hash: Some(String::new()),
                external_request_hash: None,
                body,
                synapse_name: ProofOfWeightsDataModel::NAME,
                retry_count: 0,
                slice_num: None,
                run_uid: None,
                is_tile: false,
                task_id: None,
                tile_idx: None,
                task_circuit: Some(pow_circ.clone()),
                task_inputs: Some(inputs),
                task_proof_system: Some(pow_circ.proof_system),
                retry_payload: RetryPayload::None,
                dsperse_circuit_path: None,
            });
        }

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
                task_circuit: Some(circuit.clone()),
                task_inputs: Some(rwr.inputs.clone()),
                task_proof_system: Some(circuit.proof_system),
                retry_payload: RetryPayload::Rwr(rwr),
                dsperse_circuit_path: None,
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
            let dslice_model = self.pipeline.prepare_dslice_request(
                uid,
                &dslice.circuit,
                dslice.inputs.clone(),
                None,
                &dslice.slice_num,
                &dslice.run_uid,
                dslice.proof_system,
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
                task_circuit: Some(dslice.circuit.clone()),
                task_inputs: Some(dslice.inputs.clone()),
                task_proof_system: Some(dslice.proof_system),
                retry_payload: RetryPayload::DSlice(Box::new(dslice)),
                dsperse_circuit_path: circuit_path,
            });
        }

        if !self.config.disable_benchmark
            && !benchmark_circuits.is_empty()
            && self
                .config
                .max_benchmark_concurrent
                .is_none_or(|max| self.benchmark_in_flight < max)
        {
            let weights: Vec<f64> = benchmark_circuits
                .iter()
                .map(|c| c.metadata.benchmark_choice_weight.unwrap_or(1.0))
                .collect();
            let dist = match rand::distributions::WeightedIndex::new(&weights) {
                Ok(d) => d,
                Err(_) => return None,
            };
            let circuit_idx = rand::Rng::sample(&mut rand::thread_rng(), &dist);
            let circuit = &benchmark_circuits[circuit_idx];
            let inputs = circuit
                .settings
                .get("default_input")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            match self.pipeline.prepare_benchmark_request(circuit, inputs) {
                Some(req) => {
                    let body = serde_json::json!({
                        "model_id": req.circuit.id,
                        "query_input": req.inputs,
                    });
                    return Some(DispatchedRequest {
                        request_type: RequestType::Benchmark,
                        guard_hash: Some(String::new()),
                        external_request_hash: None,
                        body,
                        synapse_name: QueryZkProof::NAME,
                        retry_count: 0,
                        slice_num: None,
                        run_uid: None,
                        is_tile: false,
                        task_id: None,
                        tile_idx: None,
                        task_circuit: Some(circuit.clone()),
                        task_inputs: Some(req.inputs),
                        task_proof_system: Some(circuit.proof_system),
                        retry_payload: RetryPayload::None,
                        dsperse_circuit_path: None,
                    });
                }
                None => return None,
            }
        }

        None
    }

    fn spawn_miner_task(
        &mut self,
        uid: u16,
        neuron: &sn2_chain::NeuronInfo,
        was_at_capacity: bool,
        timeout: f64,
        d: DispatchedRequest,
    ) {
        let ip = neuron.axon_ip.clone();
        let port = neuron.axon_port;
        let hotkey = neuron.hotkey.clone();
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
                        dsperse_slice_num: slice_num.as_deref().and_then(|s| s.parse().ok()),
                        dsperse_run_uid: run_uid.clone(),
                        raw: Some(resp_body),
                        error: None,
                        save: false,
                        computed_outputs: None,
                        is_incremental: request_type == RequestType::DSlice,
                        witness: None,
                        dsperse_circuit_path,
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
        let is_benchmark = request_type == RequestType::Benchmark;
        self.task_meta
            .insert(abort_handle.id(), (uid, task_guard_hash, is_benchmark));

        *self.miner_active_count.entry(uid).or_insert(0) += 1;
        if is_benchmark {
            self.benchmark_in_flight += 1;
        }
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

    pub(super) fn compute_api_eligible(&self, neurons: &[&sn2_chain::NeuronInfo]) -> HashSet<u16> {
        if neurons.is_empty() || self.config.api_miners_pct == 0 {
            return HashSet::new();
        }

        let snap = self.performance_tracker.snapshot();
        let queryable: HashSet<u16> = neurons.iter().map(|n| n.uid).collect();

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
}
