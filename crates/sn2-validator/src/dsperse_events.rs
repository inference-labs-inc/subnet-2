use std::sync::Arc;

use crate::stats_reporter::collect_environment;
use base64::Engine;
use sn2_chain::Wallet;
use sn2_types::DEFAULT_API_URL;
use tracing::{info, warn};

const BATCH_SIZE: usize = 20;
const MAX_BUFFERED_EVENTS: usize = 10_000;
const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct DsperseEventClient {
    http: reqwest::Client,
    wallet: Arc<Wallet>,
    api_url: String,
    buffer: tokio::sync::Mutex<Vec<serde_json::Value>>,
}

impl DsperseEventClient {
    pub fn new(wallet: Arc<Wallet>, api_url: Option<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_default(),
            wallet,
            api_url: api_url.unwrap_or_else(|| DEFAULT_API_URL.to_string()),
            buffer: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    async fn restore_buffer(&self, events: Vec<serde_json::Value>) {
        let mut buf = self.buffer.lock().await;
        let mut restored = events;
        restored.append(&mut *buf);
        if restored.len() > MAX_BUFFERED_EVENTS {
            let excess = restored.len() - MAX_BUFFERED_EVENTS;
            restored.drain(0..excess);
        }
        *buf = restored;
    }

    pub async fn emit(&self, mut event: serde_json::Value) {
        if event.get("timestamp").is_none() {
            let dur = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let secs = dur.as_secs();
            let millis = dur.subsec_millis();
            event["timestamp"] = serde_json::Value::String(format!("{secs}.{millis:03}"));
        }
        event["validator_key"] = serde_json::Value::String(self.wallet.hotkey_ss58().to_string());

        let should_flush = {
            let mut buf = self.buffer.lock().await;
            if buf.len() >= MAX_BUFFERED_EVENTS {
                let drop_n = (buf.len() + 1).saturating_sub(MAX_BUFFERED_EVENTS);
                buf.drain(0..drop_n);
                warn!(
                    dropped = drop_n,
                    "dsperse event buffer full, dropping oldest events"
                );
            }
            buf.push(event);
            buf.len() >= BATCH_SIZE
        };

        if should_flush {
            self.flush().await;
        }
    }

    pub async fn flush(&self) {
        let events = {
            let mut buf = self.buffer.lock().await;
            if buf.is_empty() {
                return;
            }
            std::mem::take(&mut *buf)
        };

        let batch = serde_json::json!({
            "validator_key": self.wallet.hotkey_ss58(),
            "events": events,
        });

        let body_bytes = match serde_json::to_vec(&batch) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "dsperse event serialization failed");
                self.restore_buffer(events).await;
                return;
            }
        };

        let sig = match self.wallet.sign_hotkey(&body_bytes) {
            Ok(s) => base64::engine::general_purpose::STANDARD.encode(&s),
            Err(e) => {
                warn!(error = %e, "dsperse event signing failed");
                self.restore_buffer(events).await;
                return;
            }
        };

        let url = format!("{}/statistics/dsperse/events/", self.api_url);
        let count = events.len();

        match self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .header("X-Request-Signature", &sig)
            .body(body_bytes)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                info!(count, "flushed dsperse events");
            }
            Ok(resp) => {
                let status = resp.status();
                warn!(%status, count, "dsperse events POST rejected");
                self.restore_buffer(events).await;
            }
            Err(e) => {
                warn!(error = %e, count, "dsperse events POST failed");
                self.restore_buffer(events).await;
            }
        }
    }

    pub fn spawn_flush_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let client = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(FLUSH_INTERVAL);
            loop {
                interval.tick().await;
                client.flush().await;
            }
        })
    }

    pub async fn emit_run_started(
        &self,
        run_uid: &str,
        circuit_id: &str,
        circuit_name: &str,
        total_slices: usize,
        total_tiles: usize,
        slice_tile_counts: &std::collections::HashMap<String, usize>,
        run_source: &str,
    ) {
        self.emit(serde_json::json!({
            "event_type": "run_started",
            "run_uid": run_uid,
            "circuit_id": circuit_id,
            "circuit_name": circuit_name,
            "total_slices": total_slices,
            "total_tiles": total_tiles,
            "slice_tile_counts": slice_tile_counts,
            "run_source": run_source,
            "environment": collect_environment(),
        }))
        .await;
    }

    pub async fn emit_work_items_created(
        &self,
        run_uid: &str,
        slice_num: &str,
        total_tiles: usize,
    ) {
        self.emit(serde_json::json!({
            "event_type": "work_items_created",
            "run_uid": run_uid,
            "slice_num": slice_num,
            "total_tiles": total_tiles,
        }))
        .await;
    }

    pub async fn emit_proof_received(
        &self,
        run_uid: &str,
        slice_num: &str,
        response_time_sec: f64,
        miner_uid: u16,
    ) {
        self.emit(serde_json::json!({
            "event_type": "proof_received",
            "run_uid": run_uid,
            "slice_num": slice_num,
            "response_time_sec": response_time_sec,
            "miner_uid": miner_uid,
        }))
        .await;
    }

    pub async fn emit_verification_complete(
        &self,
        run_uid: &str,
        slice_num: &str,
        verification_time_sec: f64,
        success: bool,
    ) {
        self.emit(serde_json::json!({
            "event_type": "verification_complete",
            "run_uid": run_uid,
            "slice_num": slice_num,
            "verification_time_sec": verification_time_sec,
            "success": success,
        }))
        .await;
    }

    pub async fn emit_slice_failed(&self, run_uid: &str, slice_num: &str, error: &str) {
        self.emit(serde_json::json!({
            "event_type": "slice_failed",
            "run_uid": run_uid,
            "slice_num": slice_num,
            "success": false,
            "error": error,
        }))
        .await;
    }

    pub async fn emit_run_complete(
        &self,
        run_uid: &str,
        all_successful: bool,
        total_run_time_sec: f64,
    ) {
        self.emit(serde_json::json!({
            "event_type": "run_complete",
            "run_uid": run_uid,
            "all_successful": all_successful,
            "total_run_time_sec": total_run_time_sec,
        }))
        .await;
    }
}
