use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use base64::Engine;
use sn2_chain::Wallet;
use sn2_types::{MinerResponse, DEFAULT_API_URL, SOFTWARE_VERSION};
use tracing::{info, warn};
const LOG_INTERVAL_SECS: u64 = 60;
const HEALTH_FLUSH_INTERVAL_SECS: u64 = 60;
const REQUEST_TIMEOUT_SECS: u64 = 5;

pub struct StatsReporter {
    http: reqwest::Client,
    wallet: Arc<Wallet>,
    api_base_url: String,
    recent_responses: Vec<serde_json::Value>,
    last_response_log: Instant,
    health_samples: Vec<HealthSample>,
    last_health_flush: Instant,
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
}

impl StatsReporter {
    pub fn new(wallet: Arc<Wallet>, api_base_url: Option<String>, validator_uid: u16) -> Self {
        let now = Instant::now();
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
                .build()
                .unwrap_or_default(),
            wallet,
            api_base_url: api_base_url.unwrap_or_else(|| DEFAULT_API_URL.to_string()),
            recent_responses: Vec::new(),
            last_response_log: now,
            health_samples: Vec::new(),
            last_health_flush: now,
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

        if now.duration_since(self.last_response_log)
            >= std::time::Duration::from_secs(LOG_INTERVAL_SECS)
            && !self.recent_responses.is_empty()
        {
            let response_logs = std::mem::take(&mut self.recent_responses);
            self.last_response_log = now;

            let overhead_duration = LOG_INTERVAL_SECS as f64;

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

        if now.duration_since(self.last_health_flush)
            >= std::time::Duration::from_secs(HEALTH_FLUSH_INTERVAL_SECS)
            && !self.health_samples.is_empty()
        {
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
                    "is_tiled": false,
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
            "environment": serde_json::json!({
                "sn2_version": SOFTWARE_VERSION,
            }),
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
            let body_bytes = match serde_json::to_vec(&body) {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, path = %path_owned, "stats serialization failed");
                    on_done(false);
                    return;
                }
            };
            let sig = match wallet.sign_hotkey(&body_bytes) {
                Ok(s) => base64::engine::general_purpose::STANDARD.encode(&s),
                Err(e) => {
                    warn!(error = %e, path = %path_owned, "stats signing failed");
                    on_done(false);
                    return;
                }
            };
            match http
                .post(&url)
                .header("Content-Type", "application/json")
                .header("X-Request-Signature", &sig)
                .body(body_bytes)
                .send()
                .await
            {
                Ok(resp) if !resp.status().is_success() => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    warn!(path = %path_owned, %status, "stats POST rejected: {text}");
                    on_done(false);
                }
                Err(e) => {
                    warn!(error = %e, path = %path_owned, "stats POST failed");
                    on_done(false);
                }
                Ok(_) => on_done(true),
            }
        });
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
