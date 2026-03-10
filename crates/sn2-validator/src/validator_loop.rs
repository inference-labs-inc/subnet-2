use std::collections::{HashMap, HashSet, VecDeque};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use btlightning::QuicAxonInfo;
use sn2_chain::WeightsSetter;
use sn2_types::*;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{watch, Notify, RwLock};
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use crate::config::ValidatorConfig;
use crate::dsperse_events::DsperseEventClient;
use crate::incremental_runner::{IncrementalRunManager, SliceArtifact};
use crate::miner_client::MinerQueryClient;
use crate::performance::PerformanceTracker;
use crate::pow_manager::{PowItem, PowManager};
use crate::proof_uploader::ProofUploader;
use crate::relay::{DsperseSubmission, RelayManager, RwrSubmission};
use crate::request_pipeline::RequestPipeline;
use crate::response_processor::ResponseProcessor;
use crate::scoring::ScoreManager;
use crate::stats_reporter::{DsperseRunReport, DsperseSliceReport, StatsReporter};
use crate::{metrics_server, metrics_server as metrics};
use sn2_circuit_store::CircuitStore;

fn event_slice_num(slice_num: &str, is_tile: bool, tile_idx: Option<u32>) -> String {
    match (is_tile, tile_idx) {
        (true, Some(idx)) => format!("{slice_num}_tile_{idx}"),
        _ => slice_num.to_string(),
    }
}

enum WeightTaskResult {
    CommitSuccess,
    CommitFailed(anyhow::Error),
}

enum RetryPayload {
    Rwr(RwrSubmission),
    DSlice(Box<DSliceRequest>),
    None,
}

struct TaskResult {
    tokio_task_id: tokio::task::Id,
    uid: u16,
    request_type: RequestType,
    guard_hash: Option<String>,
    external_request_hash: Option<String>,
    retry_count: u32,
    was_at_capacity: bool,
    slice_num: Option<String>,
    run_uid: Option<String>,
    is_tile: bool,
    task_id: Option<String>,
    tile_idx: Option<u32>,
    outcome: TaskOutcome,
    retry_payload: RetryPayload,
}

enum TaskOutcome {
    Success(Box<MinerResponse>),
    Failure(String),
}

struct VerifyResult {
    verify_task_id: tokio::task::Id,
    task_result: TaskResult,
    verified: bool,
}

pub struct ValidatorLoop {
    config: ValidatorConfig,
    score_manager: ScoreManager,
    performance_tracker: PerformanceTracker,
    weights_setter: WeightsSetter,
    miner_client: Arc<RwLock<MinerQueryClient>>,
    relay: Option<RelayManager>,
    pipeline: RequestPipeline,
    circuit_store: CircuitStore,
    tasks: JoinSet<TaskResult>,
    miner_active_count: HashMap<u16, usize>,
    api_dslice_queue: VecDeque<DSliceRequest>,
    stacked_dslice_queue: VecDeque<DSliceRequest>,
    rwr_queue: VecDeque<RwrSubmission>,
    dsperse_rx: tokio::sync::mpsc::Receiver<DsperseSubmission>,
    rwr_rx: tokio::sync::mpsc::Receiver<RwrSubmission>,
    last_metagraph_sync: Instant,
    last_weight_update: Instant,
    last_score_save: Instant,
    last_circuit_refresh: Instant,
    uid_hotkeys: HashMap<u16, String>,
    pow_manager: PowManager,
    dispatch_notify: Arc<Notify>,
    last_perf_save: Instant,
    last_health_log: Instant,
    last_replenish: Instant,
    last_gc: Instant,
    task_meta: HashMap<tokio::task::Id, (u16, Option<String>, bool)>,
    run_manager: IncrementalRunManager,
    proof_uploader: Option<Arc<ProofUploader>>,
    benchmark_in_flight: usize,
    upload_tasks: JoinSet<()>,
    weight_tasks: JoinSet<WeightTaskResult>,
    dsperse_benchmark_backoff_until: Instant,
    stats_reporter: Option<StatsReporter>,
    dsperse_events: Option<Arc<DsperseEventClient>>,
    dsperse_flush_task: Option<tokio::task::JoinHandle<()>>,
    dsperse_emit_tasks: JoinSet<()>,
    verify_tasks: JoinSet<VerifyResult>,
    verify_guard_hashes: HashMap<tokio::task::Id, Option<String>>,
    pending_verifications: VecDeque<TaskResult>,
}

impl ValidatorLoop {
    pub async fn new(config: ValidatorConfig) -> Result<Self> {
        metrics_server::init_metrics(config.metrics_port)?;

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
            last_metagraph_sync: now - Duration::from_secs(3601),
            last_weight_update: now,
            last_score_save: now,
            last_circuit_refresh: now,
            uid_hotkeys: HashMap::new(),
            pow_manager: PowManager::new(),
            dispatch_notify: Arc::new(Notify::new()),
            last_perf_save: now,
            last_health_log: now,
            last_replenish: now,
            last_gc: now,
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
                        tokio::time::sleep(Duration::from_secs(EXCEPTION_DELAY_SECONDS)).await;
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

    async fn relay_send_response(&self, request_id: &str, result: serde_json::Value) {
        if let Some(relay) = &self.relay {
            relay.send_response(request_id, result).await;
        }
    }

    async fn relay_send_notification(&self, method: &str, params: serde_json::Value) {
        if let Some(relay) = &self.relay {
            relay.send_notification(method, params).await;
        }
    }

    async fn relay_register_pending(&self, hash: &str) {
        if let Some(relay) = &self.relay {
            relay.register_pending(hash).await;
        }
    }

    async fn relay_remove_pending(&self, hash: &str) {
        if let Some(relay) = &self.relay {
            relay.remove_pending(hash).await;
        }
    }

    async fn relay_set_request_result(&self, request_hash: &str, result: serde_json::Value) {
        if let Some(relay) = &self.relay {
            relay.set_request_result(request_hash, result).await;
        }
    }

    async fn handle_dsperse_submission(&mut self, submission: DsperseSubmission) {
        let circuit = match self
            .circuit_store
            .ensure_circuit(&submission.circuit_id)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                warn!(circuit = %submission.circuit_id, error = %e, "unknown circuit in dsperse submission");
                if let Some(req_id) = &submission.request_id {
                    self.relay_send_response(
                        req_id,
                        serde_json::json!({"error": format!("unknown circuit: {e}")}),
                    )
                    .await;
                }
                return;
            }
        };

        if let Err(msg) = circuit.validate_inputs(&submission.inputs) {
            warn!(circuit = %circuit.id, error = %msg, "invalid inputs for dsperse submission");
            if let Some(req_id) = &submission.request_id {
                self.relay_send_response(
                    req_id,
                    serde_json::json!({"error": format!("invalid input shape: {msg}")}),
                )
                .await;
            }
            return;
        }

        let slices_dir = circuit.paths.base_path.join("slices");
        let input_tensor = match crate::tensor_json::json_to_arrayd(&submission.inputs) {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, "failed to convert input to tensor");
                if let Some(req_id) = &submission.request_id {
                    self.relay_send_response(req_id, serde_json::json!({"error": e.to_string()}))
                        .await;
                }
                return;
            }
        };

        let incremental = match dsperse::pipeline::IncrementalRun::new(&slices_dir, input_tensor) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, circuit = %circuit.id, "failed to create IncrementalRun");
                if let Some(req_id) = &submission.request_id {
                    self.relay_send_response(req_id, serde_json::json!({"error": e.to_string()}))
                        .await;
                }
                return;
            }
        };

        let run_uid = uuid::Uuid::new_v4().to_string();
        info!(run_uid = %run_uid, circuit = %circuit.id, "started incremental run");

        self.run_manager.start_run(
            run_uid.clone(),
            circuit.id.clone(),
            circuit.metadata.name.clone(),
            RunSource::Api,
            submission.request_id.clone(),
            Some(incremental),
        );

        let (total_slices, total_tiles, stc) = self.run_manager.slice_tile_counts(&run_uid);

        if let Some(ev) = &self.dsperse_events {
            let ev = Arc::clone(ev);
            let uid = run_uid.clone();
            let cid = circuit.id.clone();
            let cname = circuit.metadata.name.clone();
            self.dsperse_emit_tasks.spawn(async move {
                ev.emit_run_started(&uid, &cid, &cname, total_slices, total_tiles, &stc, "api")
                    .await;
            });
        }

        self.relay_register_pending(&run_uid).await;

        if let Some(req_id) = &submission.request_id {
            self.relay_send_response(
                req_id,
                serde_json::json!({
                    "run_uid": run_uid,
                    "status": "started",
                    "total_slices": total_slices,
                    "total_tiles": total_tiles,
                }),
            )
            .await;
        }

        let benchmark_uids = self.run_manager.benchmark_run_uids();
        for uid in &benchmark_uids {
            info!(run_uid = %uid, "preempting benchmark run for API dsperse submission");
            self.teardown_run(uid).await;
        }
        self.stacked_dslice_queue.clear();
        self.dsperse_benchmark_backoff_until = Instant::now() + Duration::from_secs(120);

        self.enqueue_next_dslice(&run_uid, &circuit).await;
    }

    async fn enqueue_next_dslice(&mut self, run_uid: &str, circuit: &Circuit) {
        let slices_dir = circuit.paths.base_path.join("slices");
        loop {
            let slice_info = match self.run_manager.next_slice(run_uid) {
                Ok(Some(info)) => info,
                Ok(None) => {
                    warn!(run_uid = %run_uid, "no next slice available");
                    return;
                }
                Err(e) => {
                    warn!(run_uid = %run_uid, error = %e, "next_slice failed");
                    return;
                }
            };

            {
                let dir = slices_dir.clone();
                let sid = slice_info.slice_id.clone();
                let result = tokio::task::spawn_blocking(move || {
                    sn2_circuit_store::ensure_slice_extracted(&dir, &sid)
                })
                .await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        warn!(run_uid = %run_uid, slice = %slice_info.slice_id, error = %e, "failed to extract dslice");
                        self.teardown_run(run_uid).await;
                        return;
                    }
                    Err(e) => {
                        warn!(run_uid = %run_uid, slice = %slice_info.slice_id, error = %e, "dslice extraction task panicked");
                        self.teardown_run(run_uid).await;
                        return;
                    }
                }
            }

            if !slice_info.use_circuit {
                let onnx_path = match slice_info.onnx_path {
                    Some(ref p) => p.clone(),
                    None => {
                        warn!(run_uid = %run_uid, slice = %slice_info.slice_id, "non-circuit slice has no onnx_path");
                        self.teardown_run(run_uid).await;
                        return;
                    }
                };
                let (output_data, output_shape) = if slice_info.named_inputs.len() > 1 {
                    let inputs: Vec<(String, Vec<f64>, Vec<usize>)> = slice_info
                        .named_inputs
                        .iter()
                        .map(|(name, arr)| {
                            (
                                name.clone(),
                                arr.iter().copied().collect(),
                                arr.shape().to_vec(),
                            )
                        })
                        .collect();
                    let onnx = onnx_path.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        let refs: Vec<(&str, Vec<f64>, Vec<usize>)> = inputs
                            .iter()
                            .map(|(n, d, s)| (n.as_str(), d.clone(), s.clone()))
                            .collect();
                        dsperse::backend::onnx::run_inference_multi(
                            std::path::Path::new(&onnx),
                            &refs,
                        )
                    })
                    .await;
                    match result {
                        Ok(Ok(r)) => r,
                        Ok(Err(e)) => {
                            warn!(run_uid = %run_uid, slice = %slice_info.slice_id, error = %e, "ONNX multi-input inference failed");
                            self.teardown_run(run_uid).await;
                            return;
                        }
                        Err(e) => {
                            warn!(run_uid = %run_uid, slice = %slice_info.slice_id, error = %e, "ONNX multi-input inference task panicked");
                            self.teardown_run(run_uid).await;
                            return;
                        }
                    }
                } else {
                    let input_flat: Vec<f64> = slice_info.input_tensor.iter().copied().collect();
                    let input_shape: Vec<usize> = slice_info.input_tensor.shape().to_vec();
                    let onnx = onnx_path.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        dsperse::backend::onnx::run_inference(
                            std::path::Path::new(&onnx),
                            &input_flat,
                            &input_shape,
                        )
                    })
                    .await;
                    match result {
                        Ok(Ok(r)) => r,
                        Ok(Err(e)) => {
                            warn!(run_uid = %run_uid, slice = %slice_info.slice_id, error = %e, "ONNX inference failed");
                            self.teardown_run(run_uid).await;
                            return;
                        }
                        Err(e) => {
                            warn!(run_uid = %run_uid, slice = %slice_info.slice_id, error = %e, "ONNX inference task panicked");
                            self.teardown_run(run_uid).await;
                            return;
                        }
                    }
                };
                let output_tensor = match ndarray::ArrayD::from_shape_vec(
                    ndarray::IxDyn(&output_shape),
                    output_data,
                ) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(
                            run_uid = %run_uid,
                            slice = %slice_info.slice_id,
                            output_shape = ?output_shape,
                            error = %e,
                            "ONNX output shape mismatch"
                        );
                        self.teardown_run(run_uid).await;
                        return;
                    }
                };
                info!(
                    run_uid = %run_uid,
                    slice = %slice_info.slice_id,
                    output_shape = ?output_shape,
                    "ran ONNX inference for non-circuit slice"
                );
                match self.run_manager.apply_result(
                    run_uid,
                    &slice_info.slice_id,
                    &crate::tensor_json::arrayd_to_json(&output_tensor),
                ) {
                    Ok(true) => {
                        sn2_circuit_store::cleanup_extracted_slice(
                            &slices_dir,
                            &slice_info.slice_id,
                        );
                        info!(run_uid = %run_uid, "incremental run complete after ONNX slice");
                        let active_run = self.run_manager.remove_run(run_uid);
                        if let Some(ref run) = active_run {
                            self.report_dsperse_completion(run);

                            self.spawn_emit_run_complete(run, true);
                        }
                        let notify_circuit_id = active_run
                            .as_ref()
                            .map(|r| r.circuit_id.as_str())
                            .unwrap_or_default()
                            .to_string();
                        self.relay_set_request_result(
                            run_uid,
                            serde_json::json!({"run_uid": run_uid, "status": "complete"}),
                        )
                        .await;
                        self.relay_send_notification(
                            "subnet-2.batch_completed",
                            serde_json::json!({
                                "run_uid": run_uid,
                                "circuit_id": notify_circuit_id,
                                "status": "completed",
                            }),
                        )
                        .await;
                        return;
                    }
                    Ok(false) => {
                        sn2_circuit_store::cleanup_extracted_slice(
                            &slices_dir,
                            &slice_info.slice_id,
                        );
                        continue;
                    }
                    Err(e) => {
                        sn2_circuit_store::cleanup_extracted_slice(
                            &slices_dir,
                            &slice_info.slice_id,
                        );
                        warn!(run_uid = %run_uid, error = %e, "apply_result failed for ONNX slice");
                        self.teardown_run(run_uid).await;
                        return;
                    }
                }
            }

            let run_source = self
                .run_manager
                .get_run_source(run_uid)
                .unwrap_or(RunSource::Benchmark);

            if let Some(ref tiling) = slice_info.tiling {
                let input_4d = match slice_info
                    .input_tensor
                    .into_dimensionality::<ndarray::Ix4>()
                {
                    Ok(arr) => arr,
                    Err(e) => {
                        warn!(
                            run_uid = %run_uid,
                            slice = %slice_info.slice_id,
                            error = %e,
                            "tiled slice requires 4D input"
                        );
                        self.teardown_run(run_uid).await;
                        return;
                    }
                };
                let tiles = match dsperse::pipeline::split_into_tiles(&input_4d, tiling) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(
                            run_uid = %run_uid,
                            slice = %slice_info.slice_id,
                            error = %e,
                            "split_into_tiles failed"
                        );
                        self.teardown_run(run_uid).await;
                        return;
                    }
                };

                let num_tiles = tiles.len();
                info!(
                    run_uid = %run_uid,
                    slice = %slice_info.slice_id,
                    num_tiles,
                    "dispatching spatial tiles"
                );

                if let Err(e) =
                    self.run_manager
                        .init_tile_buffer(run_uid, &slice_info.slice_id, tiling.clone())
                {
                    warn!(
                        run_uid = %run_uid,
                        slice = %slice_info.slice_id,
                        error = %e,
                        "init_tile_buffer failed"
                    );
                    self.teardown_run(run_uid).await;
                    return;
                }

                for (idx, tile) in tiles.into_iter().enumerate() {
                    let tile_json = serde_json::json!({
                        "input_data": crate::tensor_json::arrayd_to_json(&tile.into_dyn())
                    });
                    let request = DSliceRequest {
                        circuit: circuit.clone(),
                        inputs: tile_json,
                        request_type: RequestType::DSlice,
                        proof_system: circuit.proof_system,
                        slice_num: slice_info.slice_id.clone(),
                        run_uid: run_uid.to_string(),
                        outputs: None,
                        is_tile: true,
                        tile_idx: Some(idx as u32),
                        task_id: None,
                        run_source,
                        retry_count: 0,
                        circuit_path: slice_info.circuit_path.clone(),
                    };
                    match run_source {
                        RunSource::Api => self.api_dslice_queue.push_back(request),
                        RunSource::Benchmark => self.stacked_dslice_queue.push_back(request),
                    };
                }

                if let Some(ev) = &self.dsperse_events {
                    let ev = Arc::clone(ev);
                    let uid = run_uid.to_string();
                    let snum = slice_info.slice_id.clone();
                    let nt = num_tiles;
                    self.dsperse_emit_tasks.spawn(async move {
                        ev.emit_work_items_created(&uid, &snum, nt).await;
                    });
                }

                return;
            }

            let request = DSliceRequest {
                circuit: circuit.clone(),
                inputs: slice_info.inputs_json,
                request_type: RequestType::DSlice,
                proof_system: circuit.proof_system,
                slice_num: slice_info.slice_id.clone(),
                run_uid: run_uid.to_string(),
                outputs: None,
                is_tile: false,
                tile_idx: None,
                task_id: None,
                run_source,
                retry_count: 0,
                circuit_path: slice_info.circuit_path.clone(),
            };
            match request.run_source {
                RunSource::Api => self.api_dslice_queue.push_back(request),
                RunSource::Benchmark => self.stacked_dslice_queue.push_back(request),
            };
            return;
        }
    }

    async fn replenish_dslice_queues(&mut self) {
        if self.config.disable_benchmark || self.run_manager.has_benchmark_runs() {
            return;
        }
        if !self.api_dslice_queue.is_empty() {
            return;
        }
        if Instant::now() < self.dsperse_benchmark_backoff_until {
            return;
        }
        let dsperse_circuits: Vec<_> = self
            .circuit_store
            .get_dsperse_circuits()
            .into_iter()
            .filter(|c| !self.circuit_store.is_downloading(&c.id))
            .collect();
        if dsperse_circuits.is_empty() {
            return;
        }
        let idx = rand::Rng::gen_range(&mut rand::thread_rng(), 0..dsperse_circuits.len());
        let circuit = &dsperse_circuits[idx];

        let schema = match &circuit.metadata.input_schema {
            Some(s) if !s.is_empty() => s.clone(),
            _ => {
                warn!(circuit = %circuit.id, "dsperse circuit has no input_schema, cannot benchmark");
                return;
            }
        };

        let shape: Vec<usize> = match schema
            .get("shape")
            .and_then(|v| v.as_array())
            .and_then(|dims| {
                dims.iter()
                    .map(|d| d.as_u64().map(|v| v as usize))
                    .collect::<Option<Vec<_>>>()
            })
            .filter(|s| !s.is_empty() && s.iter().all(|&d| d > 0))
        {
            Some(s) => s,
            None => {
                warn!(circuit = %circuit.id, "cannot derive tensor shape from input_schema");
                return;
            }
        };

        let mut rng = rand::thread_rng();
        let input = ndarray::ArrayD::from_shape_fn(ndarray::IxDyn(&shape), |_| {
            rand::Rng::gen_range(&mut rng, 0.0_f64..1.0)
        });
        let slices_dir = circuit.paths.base_path.join("slices");

        let incremental = match dsperse::pipeline::IncrementalRun::new(&slices_dir, input) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, circuit = %circuit.id, "failed to create benchmark IncrementalRun");
                self.dsperse_benchmark_backoff_until = Instant::now() + Duration::from_secs(60);
                return;
            }
        };

        let run_uid = uuid::Uuid::new_v4().to_string();
        info!(run_uid = %run_uid, circuit = %circuit.id, name = %circuit.metadata.name, "started dsperse benchmark run");

        self.run_manager.start_run(
            run_uid.clone(),
            circuit.id.clone(),
            circuit.metadata.name.clone(),
            RunSource::Benchmark,
            None,
            Some(incremental),
        );

        if let Some(ev) = &self.dsperse_events {
            let (total_slices, total_tiles, stc) = self.run_manager.slice_tile_counts(&run_uid);
            let ev = Arc::clone(ev);
            let uid = run_uid.clone();
            let cid = circuit.id.clone();
            let cname = circuit.metadata.name.clone();
            self.dsperse_emit_tasks.spawn(async move {
                ev.emit_run_started(
                    &uid,
                    &cid,
                    &cname,
                    total_slices,
                    total_tiles,
                    &stc,
                    "benchmark",
                )
                .await;
            });
        }

        let circuit = circuit.clone();
        self.enqueue_next_dslice(&run_uid, &circuit).await;
        self.dispatch_notify.notify_one();
    }

    async fn dispatch_requests(&mut self) -> Result<()> {
        let verification_backlog = self.verify_tasks.len() + self.pending_verifications.len();
        if verification_backlog >= self.config.max_concurrent_verifications {
            return Ok(());
        }

        let active_count = self.tasks.len();
        let available_slots = self.config.max_concurrency.saturating_sub(active_count);
        if available_slots == 0 {
            return Ok(());
        }

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

        let mut dispatched = 0usize;

        for neuron in &neurons {
            if dispatched >= available_slots {
                break;
            }

            let uid = neuron.uid;
            let cap = capacities.get(&uid).copied().unwrap_or(1);
            let active_now = self.miner_active_count.get(&uid).copied().unwrap_or(0);
            if active_now >= cap {
                continue;
            }
            let slots_for_miner = (cap - active_now).min(available_slots - dispatched);

            for _slot in 0..slots_for_miner {
                if dispatched >= available_slots {
                    break;
                }
                let active = self.miner_active_count.get(&uid).copied().unwrap_or(0);
                let was_at_capacity = active + 1 >= cap;

                let request_type;
                let guard_hash;
                let external_request_hash;
                let body: serde_json::Value;
                let synapse_name: &str;
                let retry_count: u32;
                let slice_num: Option<String>;
                let run_uid: Option<String>;
                let is_tile: bool;
                let task_id: Option<String>;
                let tile_idx: Option<u32>;
                let task_circuit: Option<Circuit>;
                let task_inputs: Option<serde_json::Value>;
                let task_proof_system: Option<ProofSystem>;
                let retry_payload: RetryPayload;
                let dsperse_circuit_path: Option<String>;

                if let Some(pow_circ) = pow_circuit
                    .as_ref()
                    .filter(|_| self.pow_manager.should_batch())
                {
                    let items = self.pow_manager.drain_batch();
                    let inputs = PowManager::prepare_inputs(&items);
                    request_type = RequestType::ProofOfWeights;
                    external_request_hash = None;
                    retry_count = 0;
                    slice_num = None;
                    run_uid = None;
                    is_tile = false;
                    task_id = None;
                    tile_idx = None;
                    synapse_name = ProofOfWeightsDataModel::NAME;
                    task_circuit = Some(pow_circ.clone());
                    task_inputs = Some(inputs.clone());
                    task_proof_system = Some(pow_circ.proof_system);
                    body = serde_json::json!({
                        "subnet_uid": self.config.netuid,
                        "verification_key_hash": pow_circ.id,
                        "proof_system": pow_circ.proof_system.to_string(),
                        "inputs": inputs,
                        "proof": "",
                        "public_signals": "",
                    });
                    guard_hash = Some(String::new());
                    retry_payload = RetryPayload::None;
                    dsperse_circuit_path = None;
                } else if let Some(rwr) = self.rwr_queue.pop_front() {
                    retry_payload = RetryPayload::Rwr(rwr.clone());
                    let circuit = match self.circuit_store.ensure_circuit(&rwr.circuit_id).await {
                        Ok(c) => c,
                        Err(e) => {
                            warn!(circuit = %rwr.circuit_id, error = %e, "unknown circuit for RWR");
                            if let Some(req_id) = &rwr.request_id {
                                self.relay_send_response(
                                    req_id,
                                    serde_json::json!({"success": false, "error": format!("unknown circuit: {e}")}),
                                ).await;
                            }
                            break;
                        }
                    };
                    if let Err(msg) = circuit.validate_inputs(&rwr.inputs) {
                        warn!(circuit = %rwr.circuit_id, error = %msg, "invalid inputs for RWR");
                        if let Some(req_id) = &rwr.request_id {
                            self.relay_send_response(
                                req_id,
                                serde_json::json!({"success": false, "error": format!("invalid input shape: {msg}")}),
                            )
                            .await;
                        }
                        break;
                    }
                    request_type = RequestType::Rwr;
                    external_request_hash = rwr.request_id.clone();
                    retry_count = rwr.retry_count;
                    slice_num = None;
                    run_uid = None;
                    is_tile = false;
                    task_id = None;
                    tile_idx = None;
                    synapse_name = QueryZkProof::NAME;
                    task_circuit = Some(circuit.clone());
                    task_inputs = Some(rwr.inputs.clone());
                    task_proof_system = Some(circuit.proof_system);
                    body = serde_json::json!({
                        "model_id": circuit.id,
                        "query_input": rwr.inputs,
                    });
                    guard_hash = self.pipeline.check_hash(&body);
                    if guard_hash.is_none() {
                        self.rwr_queue.push_back(rwr);
                        break;
                    }
                    dsperse_circuit_path = None;
                } else if !self.api_dslice_queue.is_empty() {
                    let Some(dslice) = self.api_dslice_queue.pop_front() else {
                        warn!(
                            "api_dslice_queue was empty despite earlier guard; skipping dispatch"
                        );
                        continue;
                    };
                    retry_payload = RetryPayload::DSlice(Box::new(dslice.clone()));
                    request_type = RequestType::DSlice;
                    external_request_hash = None;
                    retry_count = dslice.retry_count;
                    slice_num = Some(dslice.slice_num.clone());
                    run_uid = Some(dslice.run_uid.clone());
                    is_tile = dslice.is_tile;
                    task_id = dslice.task_id.clone();
                    tile_idx = dslice.tile_idx;
                    task_circuit = Some(dslice.circuit.clone());
                    task_inputs = Some(dslice.inputs.clone());
                    task_proof_system = Some(dslice.proof_system);
                    synapse_name = DSliceProofGenerationDataModel::NAME;
                    let dslice_model = self.pipeline.prepare_dslice_request(
                        uid,
                        &dslice.circuit,
                        dslice.inputs.clone(),
                        None,
                        &dslice.slice_num,
                        &dslice.run_uid,
                        dslice.proof_system,
                    );
                    body = serde_json::to_value(&dslice_model).unwrap_or_default();
                    guard_hash = self.pipeline.check_dslice_hash(
                        &dslice.circuit.id,
                        &dslice.slice_num,
                        &dslice.run_uid,
                        dslice.tile_idx,
                    );
                    if guard_hash.is_none() {
                        self.api_dslice_queue.push_back(dslice);
                        break;
                    }
                    dsperse_circuit_path = dslice.circuit_path.clone();
                } else if let Some(dslice) = self.stacked_dslice_queue.pop_front() {
                    retry_payload = RetryPayload::DSlice(Box::new(dslice.clone()));
                    request_type = RequestType::DSlice;
                    external_request_hash = None;
                    retry_count = dslice.retry_count;
                    slice_num = Some(dslice.slice_num.clone());
                    run_uid = Some(dslice.run_uid.clone());
                    is_tile = dslice.is_tile;
                    task_id = dslice.task_id.clone();
                    tile_idx = dslice.tile_idx;
                    task_circuit = Some(dslice.circuit.clone());
                    task_inputs = Some(dslice.inputs.clone());
                    task_proof_system = Some(dslice.proof_system);
                    synapse_name = DSliceProofGenerationDataModel::NAME;
                    let dslice_model = self.pipeline.prepare_dslice_request(
                        uid,
                        &dslice.circuit,
                        dslice.inputs.clone(),
                        None,
                        &dslice.slice_num,
                        &dslice.run_uid,
                        dslice.proof_system,
                    );
                    body = serde_json::to_value(&dslice_model).unwrap_or_default();
                    guard_hash = self.pipeline.check_dslice_hash(
                        &dslice.circuit.id,
                        &dslice.slice_num,
                        &dslice.run_uid,
                        dslice.tile_idx,
                    );
                    if guard_hash.is_none() {
                        self.stacked_dslice_queue.push_back(dslice);
                        break;
                    }
                    dsperse_circuit_path = dslice.circuit_path.clone();
                } else if !self.config.disable_benchmark
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
                        Err(_) => break,
                    };
                    let circuit_idx = rand::Rng::sample(&mut rand::thread_rng(), &dist);
                    let circuit = &benchmark_circuits[circuit_idx];
                    request_type = RequestType::Benchmark;
                    external_request_hash = None;
                    retry_count = 0;
                    slice_num = None;
                    run_uid = None;
                    is_tile = false;
                    task_id = None;
                    tile_idx = None;
                    task_circuit = Some(circuit.clone());
                    task_proof_system = Some(circuit.proof_system);
                    retry_payload = RetryPayload::None;
                    synapse_name = QueryZkProof::NAME;
                    let inputs = circuit
                        .settings
                        .get("default_input")
                        .cloned()
                        .unwrap_or(serde_json::json!({}));
                    match self.pipeline.prepare_benchmark_request(circuit, inputs) {
                        Some(req) => {
                            task_inputs = Some(req.inputs.clone());
                            body = serde_json::json!({
                                "model_id": req.circuit.id,
                                "query_input": req.inputs,
                            });
                            guard_hash = Some(String::new());
                        }
                        None => break,
                    }
                    dsperse_circuit_path = None;
                } else {
                    break;
                }

                let ip = neuron.axon_ip.clone();
                let port = neuron.axon_port;
                let hotkey = neuron.hotkey.clone();
                let timeout = if api_eligible.contains(&uid) {
                    API_TIMEOUT_SECONDS
                } else {
                    adaptive_timeout
                };

                let client = Arc::clone(&self.miner_client);
                let is_loopback = self.config.loopback;

                let task_slice_num = slice_num.clone();
                let task_run_uid = run_uid.clone();
                let task_task_id = task_id.clone();
                let task_circuit_clone = task_circuit;
                let task_inputs_clone = task_inputs;
                let task_proof_system_clone = task_proof_system;
                let task_retry_payload = retry_payload;
                let task_guard_hash = guard_hash.clone();
                let task_dsperse_circuit_path = dsperse_circuit_path;

                let abort_handle = self.tasks.spawn(async move {
                    let tokio_task_id = tokio::task::id();

                    let guard = client.read().await;
                    let query_result = if is_loopback {
                        let headers = guard
                            .build_signing_headers(&body, &hotkey)
                            .unwrap_or_default();
                        guard
                            .query_miner_http(&ip, port, synapse_name, &body, &headers, timeout)
                            .await
                    } else {
                        guard
                            .query_miner(&ip, port, &hotkey, synapse_name, &body, timeout)
                            .await
                    };
                    drop(guard);

                    let outcome = match query_result {
                        Ok((resp_body, elapsed)) => {
                            let mut response = MinerResponse {
                                uid,
                                verification_result: false,
                                external_request_hash: external_request_hash
                                    .clone()
                                    .unwrap_or_default(),
                                response_time: elapsed,
                                proof_size: 0,
                                circuit: task_circuit_clone,
                                proof_system: task_proof_system_clone,
                                verification_time: None,
                                proof_content: resp_body
                                    .get("query_output")
                                    .cloned()
                                    .or_else(|| resp_body.get("proof").cloned()),
                                public_json: None,
                                inputs: task_inputs_clone,
                                request_type: Some(request_type),
                                dsperse_slice_num: task_slice_num
                                    .as_deref()
                                    .and_then(|s| s.parse().ok()),
                                dsperse_run_uid: task_run_uid.clone(),
                                raw: Some(resp_body),
                                error: None,
                                save: false,
                                computed_outputs: None,
                                is_incremental: request_type == RequestType::DSlice,
                                witness: None,
                                dsperse_circuit_path: task_dsperse_circuit_path,
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
                        external_request_hash: external_request_hash.clone(),
                        retry_count,
                        was_at_capacity,
                        slice_num,
                        run_uid,
                        is_tile,
                        task_id: task_task_id,
                        tile_idx,
                        outcome,
                        retry_payload: task_retry_payload,
                    }
                });
                let is_benchmark = request_type == RequestType::Benchmark;
                self.task_meta
                    .insert(abort_handle.id(), (uid, task_guard_hash, is_benchmark));

                *self.miner_active_count.entry(uid).or_insert(0) += 1;
                if is_benchmark {
                    self.benchmark_in_flight += 1;
                }
                dispatched += 1;
                metrics::record_request_sent(&request_type.to_string());
            } // end inner slot loop
        }

        Ok(())
    }

    fn get_queryable_neurons(&self) -> Vec<&sn2_chain::NeuronInfo> {
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

    fn compute_api_eligible(&self, neurons: &[&sn2_chain::NeuronInfo]) -> HashSet<u16> {
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

    fn start_verification(&mut self, result: TaskResult) {
        let uid = result.uid;

        if let Some(count) = self.miner_active_count.get_mut(&uid) {
            *count = count.saturating_sub(1);
        }
        if result.request_type == RequestType::Benchmark {
            self.benchmark_in_flight = self.benchmark_in_flight.saturating_sub(1);
        }

        if self.verify_tasks.len() >= self.config.max_concurrent_verifications {
            self.pending_verifications.push_back(result);
            return;
        }

        self.spawn_verification(result);
    }

    fn drain_pending_verifications(&mut self) {
        while self.verify_tasks.len() < self.config.max_concurrent_verifications {
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

    async fn finish_verification(&mut self, vr: VerifyResult, guard_hash: Option<String>) {
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
        let external_request_hash = result.external_request_hash.clone();
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

                        info!(uid = uid, elapsed = format!("{elapsed:.3}s"), rtype = %request_type, "proof verified");

                        if request_type == RequestType::DSlice {
                            if let (Some(ev), Some(ref ruid), Some(ref snum)) =
                                (&self.dsperse_events, &run_uid, &slice_num)
                            {
                                let ev = Arc::clone(ev);
                                let ruid = ruid.clone();
                                let event_snum = event_slice_num(snum, is_tile, tile_idx);
                                self.dsperse_emit_tasks.spawn(async move {
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

                        if let Some(req_id) = &external_request_hash {
                            self.relay_send_response(
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
                        if let (Some(ev), Some(ref ruid), Some(ref snum)) =
                            (&self.dsperse_events, &run_uid, &slice_num)
                        {
                            let ev = Arc::clone(ev);
                            let ruid = ruid.clone();
                            let event_snum = event_slice_num(snum, is_tile, tile_idx);
                            let vt = response.verification_time.unwrap_or(0.0);
                            self.dsperse_emit_tasks.spawn(async move {
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
                external_request_hash.as_deref(),
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

    async fn handle_pow_success(&mut self, response: &MinerResponse) {
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

    #[allow(clippy::too_many_arguments)]
    async fn handle_dslice_success(
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

        if is_tile {
            let tile_idx = match tile_idx {
                Some(idx) => idx,
                None => {
                    warn!(run_uid = %run_uid, slice = %slice_num, "tile response missing tile_idx, removing run");
                    self.teardown_run(&run_uid).await;
                    return;
                }
            };

            let computed = response
                .computed_outputs
                .as_ref()
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            let tile_output = match crate::tensor_json::json_to_arrayd(&computed) {
                Ok(t) => t,
                Err(e) => {
                    warn!(
                        run_uid = %run_uid,
                        slice = %slice_num,
                        tile_idx = tile_idx,
                        error = %e,
                        "tile output tensor conversion failed, removing run"
                    );
                    self.teardown_run(&run_uid).await;
                    return;
                }
            };

            use crate::incremental_runner::TileBufferOutcome;
            match self
                .run_manager
                .buffer_tile_result(&run_uid, &slice_num, tile_idx, tile_output)
            {
                TileBufferOutcome::Waiting => return,
                TileBufferOutcome::Ready(full_output) => {
                    let full_json = crate::tensor_json::arrayd_to_json(&full_output);
                    self.apply_dslice_result(&run_uid, &slice_num, &full_json)
                        .await;
                }
                TileBufferOutcome::Failed(reason) => {
                    warn!(
                        run_uid = %run_uid,
                        slice = %slice_num,
                        error = %reason,
                        "tile buffering failed, removing run"
                    );
                    self.teardown_run(&run_uid).await;
                }
            }
            return;
        }

        let computed = response
            .computed_outputs
            .as_ref()
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        self.apply_dslice_result(&run_uid, &slice_num, &computed)
            .await;
    }

    async fn apply_dslice_result(
        &mut self,
        run_uid: &str,
        slice_num: &str,
        computed: &serde_json::Value,
    ) {
        let slices_dir = self
            .run_manager
            .get_circuit_id(run_uid)
            .and_then(|cid| self.circuit_store.get_circuit(cid))
            .map(|c| c.paths.base_path.join("slices"));
        if let Some(ref sd) = slices_dir {
            let slice_path = sd.join(slice_num);
            sn2_verify::evict_circuit_cache(&slice_path.to_string_lossy());
            sn2_circuit_store::cleanup_extracted_slice(sd, slice_num);
        }

        match self.run_manager.apply_result(run_uid, slice_num, computed) {
            Ok(is_complete) => {
                if is_complete {
                    info!(run_uid = %run_uid, "incremental run complete");

                    let final_output = self.run_manager.final_output_json(run_uid);
                    let mut active_run = self.run_manager.remove_run(run_uid);

                    if let Some(ref run) = active_run {
                        self.report_dsperse_completion(run);
                        self.spawn_emit_run_complete(run, true);
                    }

                    let artifacts = active_run
                        .as_mut()
                        .map(|r| std::mem::take(&mut r.artifacts))
                        .unwrap_or_default();

                    if !artifacts.is_empty() {
                        if let Some(uploader) = &self.proof_uploader {
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
                    }

                    let notify_circuit_id = active_run
                        .as_ref()
                        .map(|r| r.circuit_id.as_str())
                        .unwrap_or_default()
                        .to_string();

                    self.relay_set_request_result(
                        run_uid,
                        serde_json::json!({"run_uid": run_uid, "status": "complete"}),
                    )
                    .await;
                    self.relay_send_notification(
                        "subnet-2.batch_completed",
                        serde_json::json!({
                            "run_uid": run_uid,
                            "circuit_id": notify_circuit_id,
                            "status": "completed",
                        }),
                    )
                    .await;
                } else {
                    self.enqueue_next_dslice_from_run(run_uid).await;
                }
            }
            Err(e) => {
                warn!(run_uid = %run_uid, error = %e, "failed to apply slice result, removing run");
                self.teardown_run(run_uid).await;
            }
        }
    }

    async fn enqueue_next_dslice_from_run(&mut self, run_uid: &str) {
        if !self.run_manager.has_run(run_uid) {
            return;
        }
        let circuit_id = self
            .run_manager
            .get_circuit_id(run_uid)
            .map(|s| s.to_string());
        if let Some(cid) = circuit_id {
            if let Some(circuit) = self.circuit_store.get_circuit(&cid).cloned() {
                self.enqueue_next_dslice(run_uid, &circuit).await;
            }
        }
    }

    fn report_dsperse_completion(&self, run: &crate::incremental_runner::ActiveRun) {
        let reporter = match &self.stats_reporter {
            Some(r) => r,
            None => return,
        };

        let total_run_time_sec = run.started_at.elapsed().as_secs_f64();
        let mut failed_count = 0usize;
        let model_slices = run
            .incremental
            .as_ref()
            .map(|i| i.model_meta().slices.clone());
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
            .incremental
            .as_ref()
            .map(|i| i.model_meta().slices.len())
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

    fn spawn_emit_run_complete(
        &mut self,
        run: &crate::incremental_runner::ActiveRun,
        completed: bool,
    ) {
        if let Some(ev) = &self.dsperse_events {
            let ev = Arc::clone(ev);
            let uid = run.run_uid.clone();
            let all_ok = completed
                && !run.artifacts.is_empty()
                && run.artifacts.iter().all(|a| a.proof_hex.is_some());
            let elapsed = run.started_at.elapsed().as_secs_f64();
            self.dsperse_emit_tasks.spawn(async move {
                ev.emit_run_complete(&uid, all_ok, elapsed).await;
            });
        }
    }

    async fn teardown_run(&mut self, run_uid: &str) {
        let removed = self.run_manager.remove_run(run_uid);
        if let Some(ref run) = removed {
            self.spawn_emit_run_complete(run, false);
        }
        self.stacked_dslice_queue
            .retain(|req| req.run_uid != run_uid);
        self.api_dslice_queue.retain(|req| req.run_uid != run_uid);
        self.relay_remove_pending(run_uid).await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_failure(
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
        external_request_hash: Option<&str>,
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

        if next_retry <= max_retries {
            match retry_payload {
                RetryPayload::Rwr(mut rwr) => {
                    rwr.retry_count = next_retry;
                    self.rwr_queue.push_back(rwr);
                    self.dispatch_notify.notify_one();
                }
                RetryPayload::DSlice(mut dslice) => {
                    if self.run_manager.has_run(&dslice.run_uid) {
                        dslice.retry_count = next_retry;
                        match dslice.run_source {
                            RunSource::Api => self.api_dslice_queue.push_back(*dslice),
                            RunSource::Benchmark => self.stacked_dslice_queue.push_back(*dslice),
                        }
                        self.dispatch_notify.notify_one();
                    }
                }
                RetryPayload::None => {}
            }
            return;
        }

        if request_type == RequestType::DSlice {
            if let Some(run_uid) = run_uid {
                if let (Some(ev), Some(snum)) = (&self.dsperse_events, slice_num) {
                    let ev = Arc::clone(ev);
                    let ruid = run_uid.clone();
                    let event_snum = event_slice_num(snum, is_tile, tile_idx);
                    let err = reason.to_string();
                    self.dsperse_emit_tasks.spawn(async move {
                        ev.emit_slice_failed(&ruid, &event_snum, &err).await;
                    });
                }
                warn!(run_uid = %run_uid, "dslice max retries exceeded, removing run");
                self.teardown_run(run_uid).await;
            }
        }

        if let Some(req_id) = external_request_hash {
            self.relay_send_response(
                req_id,
                serde_json::json!({
                    "success": false,
                    "error": "max retries exceeded",
                }),
            )
            .await;
        }
    }

    async fn run_periodic_tasks(&mut self) -> Result<()> {
        let now = Instant::now();

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
                            warn!(error = ?e, "chain RPC disconnected during weight commit, reconnecting");
                            if let Err(re) = self.config.reconnect_chain_client().await {
                                warn!(error = ?re, "chain reconnect failed after weight commit RPC disconnect");
                            }
                        }
                        warn!(error = ?e, "weight commit failed");
                    }
                    Err(e) => {
                        warn!(error = %e, "weight task panicked");
                    }
                }
            }

            if now.duration_since(self.last_metagraph_sync) > Duration::from_secs(3600) {
                self.sync_metagraph().await?;
                self.last_metagraph_sync = now;
            }

            if now.duration_since(self.last_weight_update)
                > Duration::from_secs(WEIGHT_UPDATE_POLL_SECS)
            {
                if self.weight_tasks.is_empty() {
                    match self.update_weights().await {
                        Ok(()) => {}
                        Err(e) => {
                            if sn2_chain::is_rpc_disconnect(&e) {
                                warn!(error = ?e, "chain RPC disconnected during weight update, reconnecting");
                                if let Err(re) = self.config.reconnect_chain_client().await {
                                    warn!(error = ?re, "chain reconnect failed after weight update RPC disconnect");
                                }
                            }
                            warn!(error = ?e, "weight update failed, will retry next cycle");
                        }
                    }
                }
                self.last_weight_update = now;
            }
        }

        if now.duration_since(self.last_score_save) > Duration::from_secs(300) {
            if let Err(e) = self.score_manager.save() {
                warn!(error = %e, "saving scores");
            }
            self.last_score_save = now;
        }

        if now.duration_since(self.last_circuit_refresh)
            > Duration::from_secs(CircuitStore::REFRESH_INTERVAL)
        {
            match self.circuit_store.refresh_circuits().await {
                Ok(removed) => {
                    for circuit_id in &removed {
                        let prefix = self.circuit_store.cache_dir().join(circuit_id);
                        sn2_verify::evict_circuit_cache(&prefix.to_string_lossy());
                        let evicted = self.run_manager.evict_by_circuit(circuit_id);
                        if !evicted.is_empty() {
                            info!(circuit = %circuit_id, runs = ?evicted, "evicted in-flight runs for deactivated circuit");
                            for run_id in &evicted {
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
                Err(e) => {
                    warn!(error = %e, "refreshing circuits");
                }
            }
            self.last_circuit_refresh = now;
        }

        if now.duration_since(self.last_perf_save) > Duration::from_secs(300) {
            self.performance_tracker.save();
            self.last_perf_save = now;
        }

        if self.api_dslice_queue.is_empty()
            && self.stacked_dslice_queue.is_empty()
            && now.duration_since(self.last_replenish) > Duration::from_secs(5)
        {
            self.replenish_dslice_queues().await;
            self.last_replenish = now;
        }

        if now.duration_since(self.last_gc) > Duration::from_secs(120) {
            let evicted = self.run_manager.gc_stale(Duration::from_secs(600));
            for uid in &evicted {
                self.relay_remove_pending(uid).await;
            }
            if !evicted.is_empty() {
                let evicted_set: HashSet<&str> = evicted.iter().map(|s| s.as_str()).collect();
                let before = self.stacked_dslice_queue.len() + self.api_dslice_queue.len();
                self.stacked_dslice_queue
                    .retain(|req| !evicted_set.contains(req.run_uid.as_str()));
                self.api_dslice_queue
                    .retain(|req| !evicted_set.contains(req.run_uid.as_str()));
                let drained =
                    before - self.stacked_dslice_queue.len() - self.api_dslice_queue.len();
                if drained > 0 {
                    info!(
                        drained = drained,
                        "drained orphaned requests from evicted runs"
                    );
                }
            }
            self.last_gc = now;
        }

        while let Some(result) = self.upload_tasks.try_join_next() {
            if let Err(e) = result {
                warn!(error = %e, "upload task panicked");
            }
        }

        while self.dsperse_emit_tasks.try_join_next().is_some() {}

        if now.duration_since(self.last_health_log) > Duration::from_secs(15) {
            let active_tasks = self.tasks.len();
            let queue_size = self.rwr_queue.len()
                + self.api_dslice_queue.len()
                + self.stacked_dslice_queue.len();
            let queryable_count = self.get_queryable_neurons().len();
            let benchmark_count = self.circuit_store.get_benchmark_circuits().len();
            let dsperse_count = self.circuit_store.get_dsperse_circuits().len();
            info!(
                active_tasks = active_tasks,
                rwr_queue = self.rwr_queue.len(),
                api_dslice_queue = self.api_dslice_queue.len(),
                stacked_dslice_queue = self.stacked_dslice_queue.len(),
                active_runs = self.run_manager.active_count(),
                queryable_neurons = queryable_count,
                benchmark_circuits = benchmark_count,
                dsperse_circuits = dsperse_count,
                max_concurrency = self.config.max_concurrency,
                max_concurrent_verifications = self.config.max_concurrent_verifications,
                benchmark_in_flight = self.benchmark_in_flight,
                "health"
            );
            if let Some(reporter) = &mut self.stats_reporter {
                reporter.sample_health(active_tasks, queue_size);
            }
            self.last_health_log = now;
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

    async fn sync_metagraph(&mut self) -> Result<()> {
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
        let (weight_uids, weights) = self
            .score_manager
            .compute_throughput_weights(&uids, &snap, owner_uid);

        if weights.iter().all(|&w| w == 0) {
            info!("no weights to set, skipping");
            return Ok(());
        }

        let version_key = WEIGHTS_VERSION as u64;
        let hotkey_bytes = wallet.hotkey_public_bytes()?.to_vec();

        let (tempo, reveal_period, current_block) = self
            .weights_setter
            .query_commit_params(chain_client)
            .await?;

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

fn is_valid_ip(ip_str: &str) -> bool {
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
