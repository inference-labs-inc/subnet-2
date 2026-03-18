mod dispatch;
mod dslice;
mod maintenance;
mod relay;
mod results;
mod verification;

use std::collections::{HashMap, VecDeque};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sn2_chain::WeightsSetter;
use sn2_types::*;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{watch, Notify, RwLock};
use tokio::task::JoinSet;
use tracing::{error, info, warn};

use crate::config::ValidatorConfig;
use crate::dsperse_events::DsperseEventClient;
use crate::incremental_runner::IncrementalRunManager;
use crate::metrics_server;
use crate::miner_client::MinerQueryClient;
use crate::performance::PerformanceTracker;
use crate::pow_manager::PowManager;
use crate::proof_uploader::ProofUploader;
use crate::relay::{DsperseSubmission, RelayManager, RwrSubmission};
use crate::request_pipeline::RequestPipeline;
use crate::scoring::ScoreManager;
use crate::stats_reporter::StatsReporter;
use sn2_circuit_store::CircuitStore;

pub(super) fn event_slice_num(slice_num: &str, is_tile: bool, tile_idx: Option<u32>) -> String {
    match (is_tile, tile_idx) {
        (true, Some(idx)) => format!("{slice_num}_tile_{idx}"),
        _ => slice_num.to_string(),
    }
}

pub(super) enum WeightTaskResult {
    CommitSuccess,
    CommitFailed(anyhow::Error),
}

pub(super) enum RetryPayload {
    Rwr(RwrSubmission),
    DSlice(Box<DSliceRequest>),
    None,
}

pub(super) struct TaskResult {
    pub(super) tokio_task_id: tokio::task::Id,
    pub(super) uid: u16,
    pub(super) request_type: RequestType,
    pub(super) guard_hash: Option<String>,
    pub(super) external_request_hash: Option<u32>,
    pub(super) retry_count: u32,
    pub(super) was_at_capacity: bool,
    pub(super) slice_num: Option<String>,
    pub(super) run_uid: Option<String>,
    pub(super) is_tile: bool,
    pub(super) task_id: Option<String>,
    pub(super) tile_idx: Option<u32>,
    pub(super) outcome: TaskOutcome,
    pub(super) retry_payload: RetryPayload,
}

pub(super) enum TaskOutcome {
    Success(Box<MinerResponse>),
    Failure(String),
}

pub(super) struct VerifyResult {
    pub(super) verify_task_id: tokio::task::Id,
    pub(super) task_result: TaskResult,
    pub(super) verified: bool,
}

pub(super) struct PeriodicTimings {
    pub(super) metagraph_sync: Instant,
    pub(super) weight_update: Instant,
    pub(super) score_save: Instant,
    pub(super) circuit_refresh: Instant,
    pub(super) perf_save: Instant,
    pub(super) health_log: Instant,
    pub(super) replenish: Instant,
    pub(super) gc: Instant,
}

impl PeriodicTimings {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            metagraph_sync: now - Duration::from_secs(3601),
            weight_update: now,
            score_save: now,
            circuit_refresh: now,
            perf_save: now,
            health_log: now,
            replenish: now,
            gc: now,
        }
    }
}

pub(super) struct DispatchedRequest {
    pub(super) request_type: RequestType,
    pub(super) guard_hash: Option<String>,
    pub(super) external_request_hash: Option<u32>,
    pub(super) body: serde_json::Value,
    pub(super) synapse_name: &'static str,
    pub(super) retry_count: u32,
    pub(super) slice_num: Option<String>,
    pub(super) run_uid: Option<String>,
    pub(super) is_tile: bool,
    pub(super) task_id: Option<String>,
    pub(super) tile_idx: Option<u32>,
    pub(super) task_circuit: Option<Circuit>,
    pub(super) task_inputs: Option<serde_json::Value>,
    pub(super) task_proof_system: Option<ProofSystem>,
    pub(super) retry_payload: RetryPayload,
    pub(super) dsperse_circuit_path: Option<String>,
}

pub struct ValidatorLoop {
    pub(super) config: ValidatorConfig,
    pub(super) score_manager: ScoreManager,
    pub(super) performance_tracker: PerformanceTracker,
    pub(super) weights_setter: WeightsSetter,
    pub(super) miner_client: Arc<RwLock<MinerQueryClient>>,
    pub(super) relay: Option<RelayManager>,
    pub(super) pipeline: RequestPipeline,
    pub(super) circuit_store: CircuitStore,
    pub(super) tasks: JoinSet<TaskResult>,
    pub(super) miner_active_count: HashMap<u16, usize>,
    pub(super) api_dslice_queue: VecDeque<DSliceRequest>,
    pub(super) stacked_dslice_queue: VecDeque<DSliceRequest>,
    pub(super) rwr_queue: VecDeque<RwrSubmission>,
    pub(super) dsperse_rx: tokio::sync::mpsc::Receiver<DsperseSubmission>,
    pub(super) rwr_rx: tokio::sync::mpsc::Receiver<RwrSubmission>,
    pub(super) timings: PeriodicTimings,
    pub(super) uid_hotkeys: HashMap<u16, String>,
    pub(super) pow_manager: PowManager,
    pub(super) dispatch_notify: Arc<Notify>,
    pub(super) task_meta: HashMap<tokio::task::Id, (u16, Option<String>, bool)>,
    pub(super) run_manager: IncrementalRunManager,
    pub(super) proof_uploader: Option<Arc<ProofUploader>>,
    pub(super) benchmark_in_flight: usize,
    pub(super) upload_tasks: JoinSet<()>,
    pub(super) weight_tasks: JoinSet<WeightTaskResult>,
    pub(super) dsperse_benchmark_backoff_until: Instant,
    pub(super) stats_reporter: Option<StatsReporter>,
    pub(super) dsperse_events: Option<Arc<DsperseEventClient>>,
    pub(super) dsperse_flush_task: Option<tokio::task::JoinHandle<()>>,
    pub(super) dsperse_emit_tasks: JoinSet<()>,
    pub(super) verify_tasks: JoinSet<VerifyResult>,
    pub(super) verify_guard_hashes: HashMap<tokio::task::Id, Option<String>>,
    pub(super) pending_verifications: VecDeque<TaskResult>,
    pub(super) verification_concurrency: usize,
    pub(super) dslice_input_scales: HashMap<(String, String), f64>,
}

impl ValidatorLoop {
    pub async fn new(config: ValidatorConfig) -> Result<Self> {
        if let Err(e) = metrics_server::init_metrics(config.metrics_port) {
            warn!(
                error = %e,
                port = config.metrics_port,
                "metrics server unavailable, continuing without prometheus"
            );
        }

        let score_path = dirs_next::home_dir()
            .unwrap_or_default()
            .join(".bittensor")
            .join("subnet-2")
            .join("scores.json");

        let score_manager = ScoreManager::new(score_path);
        let perf_path = dirs_next::home_dir()
            .unwrap_or_default()
            .join(".bittensor")
            .join("subnet-2")
            .join("performance_tracker.json");
        let performance_tracker = PerformanceTracker::new_with_persistence(perf_path);
        let weights_setter = WeightsSetter::new(config.netuid);

        let (dsperse_tx, dsperse_rx) = tokio::sync::mpsc::channel::<DsperseSubmission>(256);
        let (rwr_tx, rwr_rx) = tokio::sync::mpsc::channel::<RwrSubmission>(256);

        let (
            miner_client,
            relay,
            proof_uploader,
            stats_reporter,
            dsperse_events,
            dsperse_flush_task,
        ) = if config.loopback {
            let client = MinerQueryClient::new_unsigned()?;
            (Arc::new(RwLock::new(client)), None, None, None, None, None)
        } else {
            let wallet = config
                .wallet
                .clone()
                .ok_or_else(|| anyhow::anyhow!("wallet required in production mode"))?;
            let client = MinerQueryClient::new(wallet.clone())?;
            let relay = RelayManager::new(
                config.relay_url.clone(),
                wallet.clone(),
                config.relay_enabled,
                dsperse_tx.clone(),
                rwr_tx.clone(),
            );
            let api_reporting_enabled = IS_RELEASE_BUILD || config.proof_api_url.is_some();
            if !api_reporting_enabled {
                info!(
                    version = SOFTWARE_VERSION,
                    "sn2-api reporting disabled for non-release build"
                );
            }
            let uploader = if api_reporting_enabled {
                Some(Arc::new(ProofUploader::new(
                    wallet.clone(),
                    config.proof_api_url.clone(),
                )))
            } else {
                None
            };
            let reporter = if api_reporting_enabled && !config.disable_metric_logging {
                Some(StatsReporter::new(
                    wallet.clone(),
                    config.proof_api_url.clone(),
                    config.user_uid,
                ))
            } else {
                None
            };
            let (events, flush_task) = if api_reporting_enabled && !config.disable_metric_logging {
                let ec = Arc::new(DsperseEventClient::new(
                    wallet,
                    config.proof_api_url.clone(),
                ));
                let handle = ec.spawn_flush_loop();
                (Some(ec), Some(handle))
            } else {
                (None, None)
            };
            (
                Arc::new(RwLock::new(client)),
                Some(relay),
                uploader,
                reporter,
                events,
                flush_task,
            )
        };

        let verification_concurrency = match std::thread::available_parallelism() {
            Ok(n) => n.get(),
            Err(e) => {
                warn!(error = %e, fallback = 8, "CPU detection failed, using fallback verification concurrency");
                8
            }
        };
        info!(
            verification_concurrency,
            "initialized verification concurrency"
        );

        let pipeline = RequestPipeline::new();
        let circuit_store_loopback = config.loopback && config.circuit_api_url.is_none();
        let circuit_store = CircuitStore::new(
            config.circuit_api_url.as_deref(),
            circuit_store_loopback,
            config.additional_circuits.clone(),
        );
        let run_manager = IncrementalRunManager::new();

        let now = Instant::now();

        Ok(Self {
            config,
            score_manager,
            performance_tracker,
            weights_setter,
            miner_client,
            relay,
            pipeline,
            circuit_store,
            tasks: JoinSet::new(),
            miner_active_count: HashMap::new(),
            api_dslice_queue: VecDeque::new(),
            stacked_dslice_queue: VecDeque::new(),
            rwr_queue: VecDeque::new(),
            dsperse_rx,
            rwr_rx,
            timings: PeriodicTimings::new(now),
            uid_hotkeys: HashMap::new(),
            pow_manager: PowManager::new(),
            dispatch_notify: Arc::new(Notify::new()),
            task_meta: HashMap::new(),
            run_manager,
            proof_uploader,
            benchmark_in_flight: 0,
            upload_tasks: JoinSet::new(),
            weight_tasks: JoinSet::new(),
            dsperse_benchmark_backoff_until: now,
            stats_reporter,
            dsperse_events,
            dsperse_flush_task,
            dsperse_emit_tasks: JoinSet::new(),
            verify_tasks: JoinSet::new(),
            verify_guard_hashes: HashMap::new(),
            pending_verifications: VecDeque::new(),
            verification_concurrency,
            dslice_input_scales: HashMap::new(),
        })
    }

    pub async fn run(&mut self, mut update_shutdown_rx: watch::Receiver<bool>) -> Result<()> {
        self.circuit_store.load_circuits().await?;
        if let Some(relay) = &mut self.relay {
            relay.start().await?;
        }

        if !self.config.loopback {
            let mut client = self.miner_client.write().await;
            client
                .init_quic()
                .await
                .context("initializing QUIC endpoint")?;
        }

        info!(
            uid = self.config.user_uid,
            netuid = self.config.netuid,
            neurons = self.config.metagraph.n,
            benchmark = !self.config.disable_benchmark,
            api_pct = self.config.api_miners_pct,
            circuits = self.circuit_store.circuit_count(),
            "validator loop starting"
        );

        let mut tick =
            tokio::time::interval(Duration::from_millis((LOOP_DELAY_SECONDS * 1000.0) as u64));
        let mut sigterm = signal(SignalKind::terminate()).context("registering SIGTERM handler")?;

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(e) = self.step().await {
                        error!(error = %e, "validator step error");
                        tick.reset_after(Duration::from_secs(EXCEPTION_DELAY_SECONDS));
                    }
                }
                Some(result) = self.tasks.join_next() => {
                    match result {
                        Ok(task_result) => {
                            self.task_meta.remove(&task_result.tokio_task_id);
                            self.start_verification(task_result);
                        }
                        Err(e) => {
                            if let Some((uid, guard_hash, is_benchmark)) = self.task_meta.remove(&e.id()) {
                                warn!(uid = uid, is_benchmark = is_benchmark, "recovering leaked state from panicked task");
                                if let Some(count) = self.miner_active_count.get_mut(&uid) {
                                    *count = count.saturating_sub(1);
                                }
                                if is_benchmark {
                                    self.benchmark_in_flight = self.benchmark_in_flight.saturating_sub(1);
                                }
                                if let Some(hash) = &guard_hash {
                                    if !hash.is_empty() {
                                        self.pipeline.release_hash(hash);
                                    }
                                }
                            }
                            error!(error = %e, "task panicked");
                        }
                    }
                    self.dispatch_notify.notify_one();
                }
                Some(result) = self.verify_tasks.join_next() => {
                    match result {
                        Ok(verify_result) => {
                            let guard_hash = self.verify_guard_hashes.remove(&verify_result.verify_task_id);
                            self.finish_verification(verify_result, guard_hash.flatten()).await;
                        }
                        Err(e) => {
                            if let Some(Some(hash)) = self.verify_guard_hashes.remove(&e.id()) {
                                if !hash.is_empty() {
                                    self.pipeline.release_hash(&hash);
                                }
                            }
                            error!(error = %e, "verification task panicked");
                        }
                    }
                    self.drain_pending_verifications();
                    self.dispatch_notify.notify_one();
                }
                Some(submission) = self.dsperse_rx.recv() => {
                    self.handle_dsperse_submission(submission).await;
                    self.dispatch_notify.notify_one();
                }
                Some(rwr) = self.rwr_rx.recv() => {
                    self.rwr_queue.push_back(rwr);
                    self.dispatch_notify.notify_one();
                }
                _ = self.dispatch_notify.notified() => {
                    if let Err(e) = self.dispatch_requests().await {
                        error!(error = %e, "dispatch error on notify");
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("shutting down validator");
                    self.shutdown().await;
                    return Ok(());
                }
                _ = sigterm.recv() => {
                    info!("received SIGTERM, shutting down validator");
                    self.shutdown().await;
                    return Ok(());
                }
                _ = async { loop { update_shutdown_rx.changed().await.ok()?; if *update_shutdown_rx.borrow() { return Some(()); } } } => {
                    info!("shutting down validator for auto-update restart");
                    self.shutdown().await;
                    return Ok(());
                }
            }
        }
    }

    async fn step(&mut self) -> Result<()> {
        self.run_periodic_tasks().await?;
        self.dispatch_requests().await?;
        Ok(())
    }

    async fn shutdown(&mut self) {
        while self.dsperse_emit_tasks.join_next().await.is_some() {}
        if let Some(ev) = &self.dsperse_events {
            ev.flush().await;
        }
        if let Some(handle) = self.dsperse_flush_task.take() {
            handle.abort();
        }
        info!("draining in-flight weight tasks");
        while let Some(result) = self.weight_tasks.join_next().await {
            match result {
                Ok(WeightTaskResult::CommitSuccess) => {
                    info!("timelocked weight commit succeeded during shutdown");
                }
                Ok(WeightTaskResult::CommitFailed(e)) => {
                    warn!(error = ?e, "weight commit failed during shutdown");
                }
                Err(e) => {
                    warn!(error = %e, "weight task panicked during shutdown");
                }
            }
        }
        info!("aborting in-flight miner tasks");
        self.tasks.shutdown().await;
        info!("draining in-flight proof uploads");
        while let Some(result) = self.upload_tasks.join_next().await {
            if let Err(e) = result {
                warn!(error = %e, "upload task failed during shutdown");
            }
        }
        self.pipeline.clear_guard();
        if let Err(e) = self.score_manager.save() {
            error!(error = %e, "saving scores during shutdown");
        }
        self.performance_tracker.save();
    }
}

pub(super) fn is_valid_ip(ip_str: &str) -> bool {
    let addr: Ipv4Addr = match ip_str.parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    addr.is_global() && !addr.is_multicast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_valid_ip_public() {
        assert!(is_valid_ip("8.8.8.8"));
    }

    #[test]
    fn is_valid_ip_rejects_loopback() {
        assert!(!is_valid_ip("127.0.0.1"));
    }

    #[test]
    fn is_valid_ip_rejects_rfc1918_10() {
        assert!(!is_valid_ip("10.0.0.1"));
    }

    #[test]
    fn is_valid_ip_rejects_rfc1918_172() {
        assert!(!is_valid_ip("172.16.0.1"));
    }

    #[test]
    fn is_valid_ip_rejects_rfc1918_192() {
        assert!(!is_valid_ip("192.168.1.1"));
    }

    #[test]
    fn is_valid_ip_rejects_link_local() {
        assert!(!is_valid_ip("169.254.0.1"));
    }

    #[test]
    fn is_valid_ip_rejects_multicast() {
        assert!(!is_valid_ip("224.0.0.1"));
    }

    #[test]
    fn is_valid_ip_rejects_broadcast() {
        assert!(!is_valid_ip("255.255.255.255"));
    }

    #[test]
    fn is_valid_ip_rejects_zero_network() {
        assert!(!is_valid_ip("0.0.0.0"));
    }

    #[test]
    fn is_valid_ip_rejects_non_ipv4() {
        assert!(!is_valid_ip("not_an_ip"));
    }

    #[test]
    fn is_valid_ip_rejects_rfc1918_172_upper_bound() {
        assert!(!is_valid_ip("172.31.255.255"));
    }

    #[test]
    fn is_valid_ip_accepts_first_public_after_172_range() {
        assert!(is_valid_ip("172.32.0.1"));
    }

    #[test]
    fn is_valid_ip_accepts_last_public_before_multicast() {
        assert!(is_valid_ip("223.255.255.255"));
    }

    #[test]
    fn is_valid_ip_rejects_class_e_240() {
        assert!(!is_valid_ip("240.0.0.1"));
    }

    #[test]
    fn is_valid_ip_rejects_class_e_254() {
        assert!(!is_valid_ip("254.0.0.1"));
    }

    #[test]
    fn is_valid_ip_rejects_cgnat() {
        assert!(!is_valid_ip("100.64.0.1"));
        assert!(!is_valid_ip("100.127.255.255"));
    }

    #[test]
    fn is_valid_ip_accepts_outside_cgnat() {
        assert!(is_valid_ip("100.63.255.255"));
        assert!(is_valid_ip("100.128.0.1"));
    }

    #[test]
    fn event_slice_num_plain() {
        assert_eq!(event_slice_num("slice_0", false, None), "slice_0");
        assert_eq!(event_slice_num("slice_3", false, Some(2)), "slice_3");
        assert_eq!(event_slice_num("slice_0", true, None), "slice_0");
    }

    #[test]
    fn event_slice_num_tiled() {
        assert_eq!(event_slice_num("slice_0", true, Some(0)), "slice_0_tile_0");
        assert_eq!(event_slice_num("slice_2", true, Some(7)), "slice_2_tile_7");
    }
}
