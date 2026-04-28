use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use btlightning::QuicAxonInfo;
use sn2_types::*;
use tracing::{debug, info, warn};

use super::{is_valid_ip, ValidatorLoop, WeightTaskResult};
use crate::metrics_server as metrics;
use sn2_circuit_store::CircuitStore;

impl ValidatorLoop {
    pub(super) async fn run_periodic_tasks(&mut self) -> Result<()> {
        let now = std::time::Instant::now();

        if !self.config.loopback {
            while let Some(result) = self.weight_tasks.try_join_next() {
                match result {
                    Ok(WeightTaskResult::CommitSuccess) => {
                        self.performance_tracker.save();
                        metrics::record_weight_update();
                        info!("timelocked weights committed, chain will auto-reveal at epoch boundary");
                    }
                    Ok(WeightTaskResult::CommitFailed(e)) => {
                        if sn2_chain::is_rpc_disconnect(&e) {
                            self.handle_rpc_disconnect("weight commit").await;
                        }
                        warn!(error = ?e, "weight commit failed");
                    }
                    Err(e) => {
                        warn!(error = %e, "weight task panicked");
                    }
                }
            }

            if now.duration_since(self.timings.metagraph_sync) > Duration::from_secs(3600) {
                self.sync_metagraph().await?;
                self.timings.metagraph_sync = now;
            }

            if now.duration_since(self.timings.weight_update)
                > Duration::from_secs(WEIGHT_UPDATE_POLL_SECS)
            {
                if self.weight_tasks.is_empty() {
                    match self.update_weights().await {
                        Ok(()) => {}
                        Err(e) => {
                            if sn2_chain::is_rpc_disconnect(&e) {
                                self.handle_rpc_disconnect("weight update").await;
                            }
                            warn!(error = ?e, "weight update failed, will retry next cycle");
                        }
                    }
                }
                self.timings.weight_update = now;
            }
        }

        if now.duration_since(self.timings.score_save) > Duration::from_secs(300) {
            if let Err(e) = self.score_manager.save() {
                warn!(error = %e, "saving scores");
            }
            self.timings.score_save = now;
        }

        if now.duration_since(self.timings.circuit_refresh)
            > Duration::from_secs(CircuitStore::REFRESH_INTERVAL)
        {
            match self.circuit_store.refresh_circuits().await {
                Ok(removed) => {
                    self.evict_deactivated_circuits(&removed).await;
                }
                Err(e) => {
                    warn!(error = %e, "refreshing circuits");
                }
            }
            self.timings.circuit_refresh = now;
        }

        if now.duration_since(self.timings.perf_save) > Duration::from_secs(300) {
            self.performance_tracker.evict_all_stale();
            self.performance_tracker.save();
            self.rsv.save();
            self.timings.perf_save = now;
        }

        if self.api_dslice_queue.is_empty()
            && self.stacked_dslice_queue.is_empty()
            && now.duration_since(self.timings.replenish) > Duration::from_secs(5)
        {
            self.replenish_dslice_queues().await;
            self.timings.replenish = now;
        }

        if now.duration_since(self.timings.gc) > Duration::from_secs(120) {
            self.gc_stale_runs().await;
            self.timings.gc = now;
        }

        while let Some(result) = self.upload_tasks.try_join_next() {
            if let Err(e) = result {
                warn!(error = %e, "upload task panicked");
            }
        }

        while self.dsperse_emit_tasks.try_join_next().is_some() {}

        if now.duration_since(self.timings.health_log) > Duration::from_secs(15) {
            let active_tasks = self.tasks.len();
            let queue_size = self.rwr_queue.len()
                + self.api_dslice_queue.len()
                + self.stacked_dslice_queue.len();
            let queryable_count = self.get_queryable_neurons().len();
            let dsperse_count = self.circuit_store.get_dsperse_circuits().len();
            info!(
                active_tasks = active_tasks,
                rwr_queue = self.rwr_queue.len(),
                api_dslice_queue = self.api_dslice_queue.len(),
                stacked_dslice_queue = self.stacked_dslice_queue.len(),
                active_runs = self.run_manager.active_count(),
                queryable_neurons = queryable_count,
                dsperse_circuits = dsperse_count,
                verification_concurrency = self.verification_concurrency,
                verify_tasks = self.verify_tasks.len(),
                pending_verifications = self.pending_verifications.len(),
                "health"
            );
            if let Some(reporter) = &mut self.stats_reporter {
                reporter.sample_health(active_tasks, queue_size);
            }
            self.timings.health_log = now;
        }

        if let Some(reporter) = &mut self.stats_reporter {
            reporter.flush_if_ready(
                self.config.metagraph.block,
                self.config.metagraph.n,
                self.score_manager.scores_snapshot(),
            );
        }

        Ok(())
    }

    async fn evict_deactivated_circuits(&mut self, removed: &[String]) {
        for circuit_id in removed {
            let prefix = self
                .circuit_store
                .cache_dir()
                .join(format!("model_{circuit_id}"));
            sn2_verify::evict_circuit_cache(&prefix.to_string_lossy());
            if self.disabled_slices.remove(circuit_id).is_some() {
                info!(circuit = %circuit_id, "cleared disabled slice set for deactivated circuit");
            }
            let evicted = self.run_manager.evict_by_circuit(circuit_id);
            if !evicted.is_empty() {
                info!(circuit = %circuit_id, runs = ?evicted, "evicted in-flight runs for deactivated circuit");
                for run_id in &evicted {
                    self.cleanup_run_resources(run_id).await;
                    self.dslice_input_scales.retain(|(uid, _), _| uid != run_id);
                    self.relay_remove_pending(run_id).await;
                }
            }
            let before = self.api_dslice_queue.len() + self.stacked_dslice_queue.len();
            self.api_dslice_queue
                .retain(|r| r.circuit.id != *circuit_id);
            self.stacked_dslice_queue
                .retain(|r| r.circuit.id != *circuit_id);
            let after = self.api_dslice_queue.len() + self.stacked_dslice_queue.len();
            if before != after {
                info!(circuit = %circuit_id, drained = before - after, "drained queued dslice requests for deactivated circuit");
            }
        }
    }

    async fn gc_stale_runs(&mut self) {
        let evicted = self.run_manager.gc_stale(Duration::from_secs(600));
        for uid in &evicted {
            self.cleanup_run_resources(uid).await;
            self.dslice_input_scales
                .retain(|(run_uid, _), _| run_uid != uid);
            self.relay_remove_pending(uid).await;
        }
        if !evicted.is_empty() {
            let evicted_set: HashSet<&str> = evicted.iter().map(|s| s.as_str()).collect();
            let before = self.stacked_dslice_queue.len() + self.api_dslice_queue.len();
            self.stacked_dslice_queue
                .retain(|req| !evicted_set.contains(req.run_uid.as_str()));
            self.api_dslice_queue
                .retain(|req| !evicted_set.contains(req.run_uid.as_str()));
            let drained = before - self.stacked_dslice_queue.len() - self.api_dslice_queue.len();
            if drained > 0 {
                info!(
                    drained = drained,
                    "drained orphaned requests from evicted runs"
                );
            }
        }
    }

    pub(super) async fn sync_metagraph(&mut self) -> Result<()> {
        let chain_client = self
            .config
            .chain_client
            .as_ref()
            .context("sync_metagraph requires chain_client")?;
        let sync_result = self.config.metagraph.sync(chain_client).await;
        if let Err(ref e) = sync_result {
            if sn2_chain::is_rpc_disconnect(e) {
                warn!(error = ?e, "chain RPC connection dead, reconnecting");
                self.config.reconnect_chain_client().await?;
                let chain_client = self
                    .config
                    .chain_client
                    .as_ref()
                    .context("chain_client missing after reconnect")?;
                self.config
                    .metagraph
                    .sync(chain_client)
                    .await
                    .context("metagraph sync after reconnect")?;
            } else {
                sync_result.context("metagraph sync")?;
            }
        }

        let uids = self.config.metagraph.uids();
        self.score_manager.sync_uids(&uids);
        self.rsv
            .prune_expired(self.current_block, self.blocks_per_tempo);

        let mut axon_count = 0usize;
        for n in &self.config.metagraph.neurons {
            if !n.axon_ip.is_empty() && n.axon_port > 0 {
                axon_count += 1;
                debug!(
                    uid = n.uid,
                    ip = %n.axon_ip,
                    port = n.axon_port,
                    protocol = n.axon_protocol,
                    active = n.is_active,
                    hotkey = %n.hotkey,
                    "neuron with axon"
                );
            }
        }

        if self.config.target_uids.is_some() {
            info!(
                neurons_with_axon = axon_count,
                "target_uids set, skipping non-queryable score zeroing"
            );
        } else {
            let queryable = self.get_queryable_neurons();
            for n in &queryable {
                debug!(uid = n.uid, ip = %n.axon_ip, port = n.axon_port, protocol = n.axon_protocol, active = n.is_active, "queryable neuron");
            }
            info!(
                neurons_with_axon = axon_count,
                queryable = queryable.len(),
                "metagraph sync complete"
            );
            let queryable_uids: HashSet<u16> = queryable.iter().map(|n| n.uid).collect();
            self.score_manager.zero_non_queryable(&queryable_uids);
        }

        for neuron in &self.config.metagraph.neurons {
            if let Some(prev_hotkey) = self.uid_hotkeys.get(&neuron.uid) {
                if *prev_hotkey != neuron.hotkey {
                    info!(uid = neuron.uid, "hotkey changed, resetting performance");
                    self.performance_tracker.reset_uid(neuron.uid);
                    self.score_manager.update_score(
                        neuron.uid,
                        false,
                        0.0,
                        0.0,
                        0.0,
                        self.config.metagraph.n,
                    );
                }
            }
            self.uid_hotkeys.insert(neuron.uid, neuron.hotkey.clone());
        }

        let miner_count = self
            .config
            .metagraph
            .neurons
            .iter()
            .filter(|n| !n.validator_permit)
            .count();
        let axon_count = self
            .config
            .metagraph
            .neurons
            .iter()
            .filter(|n| !n.validator_permit && !n.axon_ip.is_empty() && n.axon_port > 0)
            .count();
        metrics::set_metagraph_n(self.config.metagraph.n);
        info!(
            n = self.config.metagraph.n,
            miners = miner_count,
            with_axon = axon_count,
            "metagraph synced"
        );

        let quic_miners: Vec<QuicAxonInfo> = self
            .config
            .metagraph
            .neurons
            .iter()
            .filter(|n| is_valid_ip(&n.axon_ip) && n.axon_port > 0)
            .map(|n| QuicAxonInfo {
                hotkey: n.hotkey.clone(),
                ip: n.axon_ip.clone(),
                port: n.axon_port,
                protocol: 4,
            })
            .collect();

        if !quic_miners.is_empty() {
            let mut client = self.miner_client.write().await;
            if let Err(e) = client
                .lightning_mut()
                .update_miner_registry(quic_miners.clone())
                .await
            {
                warn!(error = %e, "updating QUIC miner connections");
            }
        }

        Ok(())
    }

    async fn update_weights(&mut self) -> Result<()> {
        let chain_client = self
            .config
            .chain_client
            .as_ref()
            .context("update_weights requires chain_client")?;
        let wallet = self
            .config
            .wallet
            .as_ref()
            .context("update_weights requires wallet")?;

        let (tempo, reveal_period, current_block) = self
            .weights_setter
            .query_commit_params(chain_client)
            .await?;

        self.current_block = current_block;
        if tempo > 0 {
            self.blocks_per_tempo = tempo;
        }

        let blocks_since = self
            .weights_setter
            .blocks_since_last_update(chain_client, self.config.user_uid)
            .await?;

        if blocks_since < WEIGHT_RATE_LIMIT_BLOCKS {
            return Ok(());
        }

        let uids = self.config.metagraph.uids();
        let snap = self.performance_tracker.throughput_snapshot();

        let tracked: Vec<_> = snap
            .iter()
            .filter(|(_, (_, _, count))| *count >= PERFORMANCE_MIN_SAMPLES)
            .collect();
        if !tracked.is_empty() {
            let mut top: Vec<_> = tracked
                .iter()
                .map(|(&uid, &(rate, cap, _))| (uid, rate, cap, rate * cap as f64))
                .collect();
            top.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
            top.truncate(5);
            let adaptive_to = self.performance_tracker.adaptive_timeout();
            info!(
                tracked = tracked.len(),
                adaptive_timeout = format!("{adaptive_to:.1}s"),
                top5 = ?top.iter().map(|(uid, r, c, t)| format!("uid={uid} rate={r:.2} cap={c} tput={t:.2}")).collect::<Vec<_>>(),
                "throughput scoring"
            );
        }

        let owner_uid = match self.config.metagraph.query_subnet_owner(chain_client).await {
            Ok(uid) => uid,
            Err(e) => {
                warn!(error = %e, "query_subnet_owner failed, proceeding without owner weight");
                None
            }
        };
        let ip_regions: HashMap<u16, String> = self
            .config
            .metagraph
            .neurons
            .iter()
            .map(|n| (n.uid, crate::scoring::ip_region(&n.axon_ip)))
            .collect();

        let skiplisted: HashSet<u16> = uids
            .iter()
            .copied()
            .filter(|uid| {
                self.uid_hotkeys
                    .get(uid)
                    .is_some_and(|hk| !hk.is_empty() && self.rsv.is_skiplisted(hk, current_block))
            })
            .collect();
        let coldstart: HashSet<u16> = uids
            .iter()
            .copied()
            .filter(|uid| match self.uid_hotkeys.get(uid) {
                Some(hk) if !hk.is_empty() => self.rsv.is_in_coldstart(hk, current_block),
                _ => false,
            })
            .collect();

        let (weight_uids, weights) = self.score_manager.compute_throughput_weights(
            &uids,
            &snap,
            owner_uid,
            &ip_regions,
            &skiplisted,
            &coldstart,
        );

        if weights.iter().all(|&w| w == 0) {
            info!("no weights to set, skipping");
            return Ok(());
        }

        let version_key = WEIGHTS_VERSION as u64;
        let hotkey_bytes = wallet.hotkey_public_bytes()?.to_vec();

        let (ct_bytes, reveal_round) = self.weights_setter.generate_timelocked_commit(
            tempo,
            reveal_period,
            current_block,
            hotkey_bytes,
            weight_uids,
            weights,
            version_key,
        )?;

        info!(
            reveal_round = reveal_round,
            ct_len = ct_bytes.len(),
            "tlock encryption complete, submitting commit"
        );

        let setter = self.weights_setter.clone();
        let client = self
            .config
            .chain_client
            .clone()
            .context("update_weights requires chain_client")?;
        let wallet = self
            .config
            .wallet
            .clone()
            .context("update_weights requires wallet")?;

        self.weight_tasks.spawn(async move {
            match setter
                .commit_timelocked_weights(&client, &wallet, ct_bytes, reveal_round)
                .await
            {
                Ok(()) => WeightTaskResult::CommitSuccess,
                Err(e) => WeightTaskResult::CommitFailed(e),
            }
        });

        Ok(())
    }
}
