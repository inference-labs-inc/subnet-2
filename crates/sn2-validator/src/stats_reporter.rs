use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use base64::Engine;
use sn2_chain::Wallet;
use sn2_types::MinerResponse;
use tracing::{info, warn};

const DEFAULT_API_URL: &str = "https://sn2-api.inferencelabs.com";
const LOG_INTERVAL_SECS: u64 = 60;
const HEALTH_FLUSH_INTERVAL_SECS: u64 = 60;
const REQUEST_TIMEOUT_SECS: u64 = 5;

pub struct StatsReporter {
    http: reqwest::Client,
    wallet: Arc<Wallet>,
    api_base_url: String,
    recent_responses: Vec<(MinerResponse, HashMap<u16, String>)>,
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

    fn sign_body(&self, body: &[u8]) -> Result<String> {
        let sig_bytes = self.wallet.sign_hotkey(body)?;
        Ok(base64::engine::general_purpose::STANDARD.encode(&sig_bytes))
    }

    pub fn record_response(&mut self, response: MinerResponse, uid_hotkeys: &HashMap<u16, String>) {
        self.recent_responses.push((response, uid_hotkeys.clone()));
    }

    pub fn sample_health(&mut self, active_tasks: usize, queue_size: usize) {
        let rss_mb = get_rss_mb();
        self.health_samples.push(HealthSample {
            rss_mb,
            active_tasks: active_tasks as f64,
            queue_size: queue_size as f64,
        });
    }

    pub async fn flush_if_ready(
        &mut self,
        block: u64,
        _metagraph_n: u16,
        scores: &HashMap<u16, f64>,
    ) {
        let now = Instant::now();

        if now.duration_since(self.last_response_log)
            >= std::time::Duration::from_secs(LOG_INTERVAL_SECS)
            && !self.recent_responses.is_empty()
        {
            let responses = std::mem::take(&mut self.recent_responses);
            self.last_response_log = now;

            let overhead_duration = LOG_INTERVAL_SECS as f64;

            let hotkey_map: HashMap<u16, String> = responses
                .first()
                .map(|(_, m)| m.clone())
                .unwrap_or_default();

            let response_logs: Vec<serde_json::Value> = responses
                .iter()
                .map(|(r, uid_map)| {
                    let miner_key = uid_map.get(&r.uid).cloned().unwrap_or_default();
                    let proof_model = r
                        .circuit
                        .as_ref()
                        .map(|c| c.metadata.name.as_str())
                        .unwrap_or("Unknown");
                    let proof_system = r
                        .proof_system
                        .map(|ps| ps.to_string())
                        .unwrap_or_else(|| "Unknown".to_string());
                    let request_type = r.request_type.map(|rt| rt.to_string());
                    serde_json::json!({
                        "miner_key": miner_key,
                        "miner_uid": r.uid,
                        "proof_model": proof_model,
                        "proof_system": proof_system,
                        "proof_size": r.proof_size,
                        "response_duration": r.response_time,
                        "is_verified": r.verification_result,
                        "external_request_hash": r.external_request_hash,
                        "request_type": request_type,
                        "error": r.error,
                        "save": r.save,
                    })
                })
                .collect();

            let scores_map: serde_json::Map<String, serde_json::Value> = scores
                .iter()
                .filter(|(_, &v)| v > 0.0)
                .map(|(&uid, &v)| (uid.to_string(), serde_json::Value::from(v)))
                .collect();

            let body = serde_json::json!({
                "validator_key": self.wallet.hotkey_ss58(),
                "validator_uid": self.validator_uid,
                "overhead_duration": overhead_duration,
                "block": block,
                "responses": response_logs,
                "scores": scores_map,
            });

            if let Err(e) = self.post("/statistics/log/", &body).await {
                warn!(error = %e, "responses log POST failed");
            } else {
                info!(count = response_logs.len(), "submitted response stats");
            }

            drop(hotkey_map);
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
            });

            if let Err(e) = self.post("/statistics/health/log/", &body).await {
                warn!(error = %e, "health metrics POST failed");
            }
        }
    }

    pub async fn report_dsperse_run(&self, report: DsperseRunReport) {
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

        let circuit_slices = report.slices.iter().filter(|s| s.success).count();
        let onnx_slices = report.total_slices.saturating_sub(circuit_slices).max(0);

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
            "environment": serde_json::Value::Null,
            "slices": slices,
        });

        if let Err(e) = self.post("/statistics/dsperse/log/", &body).await {
            warn!(run_uid = %report.run_uid, error = %e, "dsperse run log POST failed");
        } else {
            info!(run_uid = %report.run_uid, "submitted dsperse run stats");
        }
    }

    async fn post(&self, path: &str, body: &serde_json::Value) -> Result<()> {
        let body_bytes = serde_json::to_vec(body)?;
        let sig = self.sign_body(&body_bytes)?;

        let resp = self
            .http
            .post(format!("{}{}", self.api_base_url, path))
            .header("Content-Type", "application/json")
            .header("X-Request-Signature", sig)
            .body(body_bytes)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("{path} returned {status}: {text}");
        }
        Ok(())
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
        use std::process::Command;
        let pid = std::process::id();
        if let Ok(output) = Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
        {
            let kb: f64 = String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse()
                .unwrap_or(0.0);
            return kb / 1024.0;
        }
        0.0
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0.0
    }
}
