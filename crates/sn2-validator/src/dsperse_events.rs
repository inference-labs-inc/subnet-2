use std::sync::Arc;

use crate::stats_reporter::{
    collect_environment, sign_and_post, FlushOffset, STATS_FLUSH_INTERVAL_SECS,
};
use sn2_chain::Wallet;
use sn2_types::DEFAULT_API_URL;
use tracing::{debug, warn};

const MAX_BUFFERED_EVENTS: usize = 10_000;
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

        let url = format!("{}/statistics/dsperse/events/", self.api_url);
        let count = events.len();

        if sign_and_post(&self.http, &self.wallet, &url, &batch, "dsperse/events").await {
            debug!(count, "flushed dsperse events");
        } else {
            self.restore_buffer(events).await;
        }
    }

    pub fn spawn_flush_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let client = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(
                FlushOffset::DsperseEvents as u64,
            ))
            .await;
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(STATS_FLUSH_INTERVAL_SECS));
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
