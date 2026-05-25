use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use sn2_chain::Wallet;
use sn2_types::{MinerResponse, DEFAULT_API_URL, SOFTWARE_VERSION};
use tracing::{debug, info, warn};

use crate::performance::{CapDirection, CapEvent};
pub(crate) const STATS_FLUSH_INTERVAL_SECS: u64 = 60;
const REQUEST_TIMEOUT_SECS: u64 = 5;
const PENDING_CAP_EVENTS_MAX: usize = 4096;

pub(crate) enum FlushOffset {
    ResponseLog = 0,
    Health = 20,
    DsperseEvents = 40,
    Capacity = 50,
}
const MAX_RETRIES: u32 = 3;
const BACKOFF_BASE_MS: u64 = 500;
const BACKOFF_MAX_MS: u64 = 8_000;

pub struct StatsReporter {
    http: reqwest::Client,
    wallet: Arc<Wallet>,
    api_base_url: String,
    recent_responses: Vec<serde_json::Value>,
    last_response_log: Instant,
    health_samples: Vec<HealthSample>,
    last_health_flush: Instant,
    last_capacity_flush: Instant,
    pending_cap_events: Arc<Mutex<VecDeque<CapEvent>>>,
    validator_uid: u16,
}

struct HealthSample {
    rss_mb: f64,
    active_tasks: f64,
    queue_size: f64,
}

pub struct DsperseRunReport {
    pub run_uid: String,
    pub circuit_id: String,
    pub circuit_name: String,
    pub total_slices: usize,
    pub total_run_time_sec: f64,
    pub all_successful: bool,
    pub failed_slice_count: usize,
    pub slices: Vec<DsperseSliceReport>,
}

pub struct DsperseSliceReport {
    pub slice_num: String,
    pub proof_system: String,
    pub response_time_sec: f64,
    pub verification_time_sec: f64,
    pub success: bool,
    pub is_tiled: bool,
    pub tile_count: Option<usize>,
}

impl StatsReporter {
    pub fn new(wallet: Arc<Wallet>, api_base_url: Option<String>, validator_uid: u16) -> Self {
        let now = Instant::now();
        let response_offset = Duration::from_secs(
            STATS_FLUSH_INTERVAL_SECS.saturating_sub(FlushOffset::ResponseLog as u64),
        );
        let health_offset = Duration::from_secs(
            STATS_FLUSH_INTERVAL_SECS.saturating_sub(FlushOffset::Health as u64),
        );
        let capacity_offset = Duration::from_secs(
            STATS_FLUSH_INTERVAL_SECS.saturating_sub(FlushOffset::Capacity as u64),
        );
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
                .build()
                .unwrap_or_default(),
            wallet,
            api_base_url: api_base_url.unwrap_or_else(|| DEFAULT_API_URL.to_string()),
            recent_responses: Vec::new(),
            last_response_log: now - response_offset,
            health_samples: Vec::new(),
            last_health_flush: now - health_offset,
            last_capacity_flush: now - capacity_offset,
            pending_cap_events: Arc::new(Mutex::new(VecDeque::new())),
            validator_uid,
        }
    }

    pub fn record_response(
        &mut self,
        response: &MinerResponse,
        uid_hotkeys: &HashMap<u16, String>,
    ) {
        let miner_key = uid_hotkeys.get(&response.uid).cloned().unwrap_or_default();
        let proof_model = response
            .circuit
            .as_ref()
            .map(|c| c.metadata.name.as_str())
            .unwrap_or("Unknown");
        let proof_system = response
            .proof_system
            .map(|ps| ps.to_string())
            .unwrap_or_else(|| "Unknown".to_string());
        let request_type = response.request_type.map(|rt| rt.to_string());
        self.recent_responses.push(serde_json::json!({
            "miner_key": miner_key,
            "miner_uid": response.uid,
            "proof_model": proof_model,
            "proof_system": proof_system,
            "proof_size": response.proof_size,
            "response_duration": response.response_time,
            "is_verified": response.verification_result,
            "external_request_hash": response.external_request_hash,
            "request_type": request_type,
            "error": response.error,
            "save": response.save,
        }));
    }

    pub fn sample_health(&mut self, active_tasks: usize, queue_size: usize) {
        let rss_mb = get_rss_mb();
        self.health_samples.push(HealthSample {
            rss_mb,
            active_tasks: active_tasks as f64,
            queue_size: queue_size as f64,
        });
    }

    pub fn flush_if_ready(&mut self, block: u64, _metagraph_n: u16, scores: &HashMap<u16, f64>) {
        let now = Instant::now();
        self.flush_response_logs(now, block, scores);
        self.flush_health_samples(now);
    }

    fn flush_response_logs(&mut self, now: Instant, block: u64, scores: &HashMap<u16, f64>) {
        if now.duration_since(self.last_response_log)
            < Duration::from_secs(STATS_FLUSH_INTERVAL_SECS)
            || self.recent_responses.is_empty()
        {
            return;
        }

        let response_logs = std::mem::take(&mut self.recent_responses);
        self.last_response_log = now;

        let overhead_duration = STATS_FLUSH_INTERVAL_SECS as f64;

        let scores_map: serde_json::Map<String, serde_json::Value> = scores
            .iter()
            .filter(|(_, &v)| v > 0.0)
            .map(|(&uid, &v)| (uid.to_string(), serde_json::Value::from(v)))
            .collect();

        let count = response_logs.len();
        let body = serde_json::json!({
            "validator_key": self.wallet.hotkey_ss58(),
            "validator_uid": self.validator_uid,
            "overhead_duration": overhead_duration,
            "block": block,
            "responses": response_logs,
            "scores": scores_map,
            "software_version": SOFTWARE_VERSION,
        });

        self.spawn_post("/statistics/log/", body, move |ok| {
            if ok {
                info!(count, "submitted response stats");
            }
        });
    }

    fn flush_health_samples(&mut self, now: Instant) {
        if now.duration_since(self.last_health_flush)
            < Duration::from_secs(STATS_FLUSH_INTERVAL_SECS)
            || self.health_samples.is_empty()
        {
            return;
        }

        let samples = std::mem::take(&mut self.health_samples);
        self.last_health_flush = now;
        let count = samples.len() as f64;

        let avg_rss_mb = samples.iter().map(|s| s.rss_mb).sum::<f64>() / count;
        let min_rss_mb = samples.iter().map(|s| s.rss_mb).fold(f64::MAX, f64::min);
        let max_rss_mb = samples.iter().map(|s| s.rss_mb).fold(0.0f64, f64::max);
        let avg_active_tasks = samples.iter().map(|s| s.active_tasks).sum::<f64>() / count;
        let avg_queue_size = samples.iter().map(|s| s.queue_size).sum::<f64>() / count;

        let body = serde_json::json!({
            "validator_key": self.wallet.hotkey_ss58(),
            "validator_uid": self.validator_uid,
            "sample_count": samples.len(),
            "avg_rss_mb": avg_rss_mb,
            "min_rss_mb": min_rss_mb,
            "max_rss_mb": max_rss_mb,
            "avg_tensor_cache_keys": 0.0,
            "avg_timing_entries": 0.0,
            "avg_active_tasks": avg_active_tasks,
            "avg_current_concurrency": avg_active_tasks,
            "avg_queue_size": avg_queue_size,
            "software_version": SOFTWARE_VERSION,
        });

        self.spawn_post("/statistics/health/log/", body, |_| {});
    }

    pub fn capacity_flush_due(&self) -> bool {
        Instant::now().duration_since(self.last_capacity_flush)
            >= Duration::from_secs(STATS_FLUSH_INTERVAL_SECS)
    }

    pub fn flush_capacity(
        &mut self,
        block: u64,
        snapshot: HashMap<u16, usize>,
        new_events: Vec<CapEvent>,
        uid_hotkeys: &HashMap<u16, String>,
    ) {
        let now = Instant::now();
        if now.duration_since(self.last_capacity_flush)
            < Duration::from_secs(STATS_FLUSH_INTERVAL_SECS)
        {
            // Caller drained the tracker but the rate-limit window has not
            // elapsed; persist the events so the next flush picks them up
            // instead of dropping a discrete state transition on the floor.
            requeue_cap_events(
                &self.pending_cap_events,
                new_events,
                RequeueReason::RateLimited,
            );
            return;
        }

        let mut events: Vec<CapEvent> = {
            let mut pending = match self.pending_cap_events.lock() {
                Ok(g) => g,
                Err(poisoned) => {
                    warn!(error = %poisoned, "cap event pending lock poisoned, recovering");
                    poisoned.into_inner()
                }
            };
            let mut combined: Vec<CapEvent> = pending.drain(..).collect();
            combined.extend(new_events);
            combined
        };

        if snapshot.is_empty() && events.is_empty() {
            return;
        }
        self.last_capacity_flush = now;

        let snapshots_payload: Vec<serde_json::Value> = snapshot
            .into_iter()
            .map(|(uid, cap)| {
                serde_json::json!({
                    "miner_uid": uid,
                    "miner_key": uid_hotkeys.get(&uid).cloned().unwrap_or_default(),
                    "cap": cap,
                })
            })
            .collect();

        let events_payload: Vec<serde_json::Value> = events
            .iter()
            .map(|e| {
                let direction = match e.direction {
                    CapDirection::Ramp => "ramp",
                    CapDirection::Backoff => "backoff",
                    CapDirection::Evict => "evict",
                    CapDirection::Rehab => "rehab",
                };
                let elapsed_ms = now.saturating_duration_since(e.at).as_millis() as u64;
                serde_json::json!({
                    "miner_uid": e.uid,
                    "miner_key": e.hotkey,
                    "direction": direction,
                    "cap_from": e.cap_from,
                    "cap_to": e.cap_to,
                    "success_rate": e.success_rate,
                    "elapsed_ms": elapsed_ms,
                })
            })
            .collect();

        let snapshot_count = snapshots_payload.len();
        let event_count = events_payload.len();
        let body = serde_json::json!({
            "validator_key": self.wallet.hotkey_ss58(),
            "validator_uid": self.validator_uid,
            "block": block,
            "snapshots": snapshots_payload,
            "events": events_payload,
            "software_version": SOFTWARE_VERSION,
        });

        let pending = Arc::clone(&self.pending_cap_events);
        events.shrink_to_fit();
        let inflight = std::mem::take(&mut events);
        self.spawn_post("/statistics/capacity/log/", body, move |ok| {
            if ok {
                info!(
                    snapshots = snapshot_count,
                    events = event_count,
                    "submitted capacity stats"
                );
            } else {
                requeue_cap_events(&pending, inflight, RequeueReason::PostFailure);
            }
        });
    }

    pub fn report_dsperse_run(&self, report: DsperseRunReport) {
        let slices: Vec<serde_json::Value> = report
            .slices
            .iter()
            .map(|s| {
                serde_json::json!({
                    "slice_num": s.slice_num,
                    "proof_system": s.proof_system,
                    "backend_used": s.proof_system,
                    "witness_time_sec": 0.0,
                    "response_time_sec": s.response_time_sec,
                    "verification_time_sec": s.verification_time_sec,
                    "is_tiled": s.is_tiled,
                    "tile_count": s.tile_count,
                    "success": s.success,
                })
            })
            .collect();

        let circuit_slices = report.slices.len();
        let onnx_slices = report.total_slices.saturating_sub(circuit_slices);

        let run_uid = report.run_uid.clone();
        let body = serde_json::json!({
            "run_uid": report.run_uid,
            "validator_key": self.wallet.hotkey_ss58(),
            "circuit_id": report.circuit_id,
            "circuit_name": report.circuit_name,
            "total_slices": report.total_slices,
            "circuit_slices": circuit_slices,
            "onnx_slices": onnx_slices,
            "total_witness_time_sec": 0.0,
            "total_response_time_sec": report.slices.iter().map(|s| s.response_time_sec).sum::<f64>(),
            "total_verification_time_sec": report.slices.iter().map(|s| s.verification_time_sec).sum::<f64>(),
            "total_run_time_sec": report.total_run_time_sec,
            "all_successful": report.all_successful,
            "failed_slice_count": report.failed_slice_count,
            "environment": collect_environment(),
            "software_version": SOFTWARE_VERSION,
            "slices": slices,
        });

        self.spawn_post("/statistics/dsperse/log/", body, move |ok| {
            if ok {
                info!(run_uid = %run_uid, "submitted dsperse run stats");
            }
        });
    }

    fn spawn_post(
        &self,
        path: &str,
        body: serde_json::Value,
        on_done: impl FnOnce(bool) + Send + 'static,
    ) {
        let http = self.http.clone();
        let wallet = Arc::clone(&self.wallet);
        let url = format!("{}{}", self.api_base_url, path);
        let path_owned = path.to_string();

        tokio::spawn(async move {
            on_done(sign_and_post(&http, &wallet, &url, &body, &path_owned).await);
        });
    }
}

#[derive(Clone, Copy, Debug)]
enum RequeueReason {
    PostFailure,
    RateLimited,
}

fn requeue_cap_events(
    pending: &Mutex<VecDeque<CapEvent>>,
    events: Vec<CapEvent>,
    reason: RequeueReason,
) {
    if events.is_empty() {
        return;
    }
    let mut p = match pending.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            warn!(error = %poisoned, "cap event re-queue lock poisoned, recovering");
            poisoned.into_inner()
        }
    };
    let count = events.len();
    for e in events {
        p.push_back(e);
    }
    while p.len() > PENDING_CAP_EVENTS_MAX {
        p.pop_front();
    }
    let pending_len = p.len();
    match reason {
        RequeueReason::PostFailure => warn!(
            count,
            pending = pending_len,
            "cap event POST failed, re-queued for next flush"
        ),
        RequeueReason::RateLimited => debug!(
            count,
            pending = pending_len,
            "cap event flush deferred by rate limit, re-queued"
        ),
    }
}

pub(crate) async fn sign_and_post(
    http: &reqwest::Client,
    wallet: &Wallet,
    url: &str,
    body: &serde_json::Value,
    label: &str,
) -> bool {
    let body_bytes = match serde_json::to_vec(body) {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, path = label, "serialization failed");
            return false;
        }
    };
    let sig = match wallet.sign_hotkey(&body_bytes) {
        Ok(s) => base64::engine::general_purpose::STANDARD.encode(&s),
        Err(e) => {
            warn!(error = %e, path = label, "signing failed");
            return false;
        }
    };
    post_with_backoff(http, url, &body_bytes, &sig, label).await
}

async fn post_with_backoff(
    http: &reqwest::Client,
    url: &str,
    body_bytes: &[u8],
    signature: &str,
    label: &str,
) -> bool {
    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let delay_ms = (BACKOFF_BASE_MS * 2u64.pow(attempt - 1)).min(BACKOFF_MAX_MS);
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        match http
            .post(url)
            .header("Content-Type", "application/json")
            .header("X-Request-Signature", signature)
            .body(body_bytes.to_vec())
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => return true,
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if attempt == MAX_RETRIES {
                    warn!(%status, path = label, "POST rejected after {} attempts: {text}", MAX_RETRIES + 1);
                } else {
                    warn!(%status, path = label, attempt = attempt + 1, "POST rejected, retrying: {text}");
                }
            }
            Err(e) => {
                if attempt == MAX_RETRIES {
                    warn!(error = %e, path = label, "POST failed after {} attempts", MAX_RETRIES + 1);
                } else {
                    warn!(error = %e, path = label, attempt = attempt + 1, "POST failed, retrying");
                }
            }
        }
    }
    false
}

pub fn collect_environment() -> serde_json::Value {
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    let (total_memory_gb, available_memory_gb) = get_memory_gb();
    let disk_free_gb = get_disk_free_gb();

    serde_json::json!({
        "sn2_version": SOFTWARE_VERSION,
        "jst_installed": true,
        "cpu_count": cpu_count,
        "total_memory_gb": total_memory_gb,
        "available_memory_gb": available_memory_gb,
        "disk_free_gb": disk_free_gb,
    })
}

fn get_memory_gb() -> (Option<f64>, Option<f64>) {
    #[cfg(target_os = "linux")]
    {
        let mut total = None;
        let mut available = None;
        if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
            for line in contents.lines() {
                if let Some(val) = line.strip_prefix("MemTotal:") {
                    total = val
                        .trim()
                        .trim_end_matches("kB")
                        .trim()
                        .parse::<f64>()
                        .ok()
                        .map(|kb| kb / (1024.0 * 1024.0));
                } else if let Some(val) = line.strip_prefix("MemAvailable:") {
                    available = val
                        .trim()
                        .trim_end_matches("kB")
                        .trim()
                        .parse::<f64>()
                        .ok()
                        .map(|kb| kb / (1024.0 * 1024.0));
                }
                if total.is_some() && available.is_some() {
                    break;
                }
            }
        }
        (total, available)
    }
    #[cfg(not(target_os = "linux"))]
    {
        (None, None)
    }
}

fn get_disk_free_gb() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let path = std::ffi::CString::new("/").ok()?;
        unsafe {
            let mut stat: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(path.as_ptr(), &mut stat) == 0 {
                let free_bytes = stat.f_bavail * stat.f_frsize;
                Some(free_bytes as f64 / (1024.0 * 1024.0 * 1024.0))
            } else {
                None
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn get_rss_mb() -> f64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(contents) = std::fs::read_to_string("/proc/self/status") {
            for line in contents.lines() {
                if let Some(val) = line.strip_prefix("VmRSS:") {
                    let kb: f64 = val
                        .trim()
                        .trim_end_matches("kB")
                        .trim()
                        .parse()
                        .unwrap_or(0.0);
                    return kb / 1024.0;
                }
            }
        }
        0.0
    }
    #[cfg(target_os = "macos")]
    {
        #[repr(C)]
        struct ProcTaskInfo {
            pti_virtual_size: u64,
            pti_resident_size: u64,
            _pti_total_user: u64,
            _pti_total_system: u64,
            _pti_threads_user: u64,
            _pti_threads_system: u64,
            _pti_policy: i32,
            _pti_faults: i32,
            _pti_pageins: i32,
            _pti_cow_faults: i32,
            _pti_messages_sent: i32,
            _pti_messages_received: i32,
            _pti_syscalls_mach: i32,
            _pti_syscalls_unix: i32,
            _pti_csw: i32,
            _pti_threadnum: i32,
            _pti_numrunning: i32,
            _pti_priority: i32,
        }

        extern "C" {
            fn proc_pidinfo(
                pid: i32,
                flavor: i32,
                arg: u64,
                buffer: *mut std::ffi::c_void,
                buffersize: i32,
            ) -> i32;
        }

        const PROC_PIDTASKINFO: i32 = 4;

        unsafe {
            let mut info = std::mem::MaybeUninit::<ProcTaskInfo>::zeroed();
            let size = std::mem::size_of::<ProcTaskInfo>() as i32;
            let ret = proc_pidinfo(
                std::process::id() as i32,
                PROC_PIDTASKINFO,
                0,
                info.as_mut_ptr() as *mut std::ffi::c_void,
                size,
            );
            if ret == size {
                let info = info.assume_init();
                return info.pti_resident_size as f64 / (1024.0 * 1024.0);
            }
        }
        0.0
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::performance::CapDirection;

    fn ev(uid: u16, hotkey: &str) -> CapEvent {
        CapEvent {
            uid,
            hotkey: hotkey.to_string(),
            direction: CapDirection::Ramp,
            cap_from: 1,
            cap_to: 2,
            success_rate: 1.0,
            at: Instant::now(),
        }
    }

    #[test]
    fn requeue_preserves_order() {
        let pending = Mutex::new(VecDeque::new());
        let batch = vec![ev(1, "a"), ev(2, "b"), ev(3, "c")];
        requeue_cap_events(&pending, batch, RequeueReason::PostFailure);
        let p = pending.lock().unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].uid, 1);
        assert_eq!(p[1].uid, 2);
        assert_eq!(p[2].uid, 3);
    }

    #[test]
    fn requeue_appends_so_older_failures_drain_first() {
        let pending = Mutex::new(VecDeque::new());
        requeue_cap_events(&pending, vec![ev(10, "a")], RequeueReason::PostFailure);
        requeue_cap_events(&pending, vec![ev(11, "b")], RequeueReason::PostFailure);
        let p = pending.lock().unwrap();
        assert_eq!(p[0].uid, 10, "earlier failure stays at the front");
        assert_eq!(p[1].uid, 11);
    }

    #[test]
    fn requeue_drops_oldest_when_buffer_full() {
        let pending = Mutex::new(VecDeque::new());
        let total = PENDING_CAP_EVENTS_MAX + 50;
        let big: Vec<CapEvent> = (0..total).map(|i| ev(i as u16, "a")).collect();
        requeue_cap_events(&pending, big, RequeueReason::PostFailure);
        let p = pending.lock().unwrap();
        assert_eq!(p.len(), PENDING_CAP_EVENTS_MAX);
        let actual: Vec<u16> = p.iter().map(|e| e.uid).collect();
        let expected: Vec<u16> = ((total - PENDING_CAP_EVENTS_MAX)..total)
            .map(|i| i as u16)
            .collect();
        assert_eq!(
            actual, expected,
            "buffer should retain the most recent PENDING_CAP_EVENTS_MAX entries in original order"
        );
    }

    #[test]
    fn requeue_no_op_on_empty_input() {
        let pending = Mutex::new(VecDeque::new());
        requeue_cap_events(&pending, Vec::new(), RequeueReason::PostFailure);
        assert!(pending.lock().unwrap().is_empty());
    }
}
