use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sn2_chain::Wallet;
use tokio::sync::{Mutex, Notify, RwLock, Semaphore};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use sn2_types::{
    RELAY_AUTH_TIMEOUT, RELAY_PING_INTERVAL, RELAY_PING_TIMEOUT, RELAY_RECONNECT_BASE_DELAY,
    RELAY_RECONNECT_MAX_DELAY,
};

type PendingMap = Arc<RwLock<HashMap<String, Arc<Mutex<PendingRequest>>>>>;

pub(crate) struct PendingRequest {
    result: Option<serde_json::Value>,
    notify: Arc<Notify>,
}

#[derive(Debug, Clone)]
pub struct DsperseSubmission {
    pub circuit_id: String,
    pub inputs: serde_json::Value,
    pub request_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RwrSubmission {
    pub circuit_id: String,
    pub inputs: serde_json::Value,
    pub request_id: Option<String>,
    pub retry_count: u32,
}

const DSPERSE_SEMAPHORE_CAPACITY: usize = 64;

pub struct RelayManager {
    relay_url: String,
    wallet: Arc<Wallet>,
    pending: Arc<RwLock<HashMap<String, Arc<Mutex<PendingRequest>>>>>,
    ws_tx: Option<tokio::sync::mpsc::Sender<Message>>,
    dsperse_tx: tokio::sync::mpsc::Sender<DsperseSubmission>,
    rwr_tx: tokio::sync::mpsc::Sender<RwrSubmission>,
    dsperse_semaphore: Arc<Semaphore>,
    enabled: bool,
}

impl RelayManager {
    pub fn new(
        relay_url: String,
        wallet: Arc<Wallet>,
        enabled: bool,
        dsperse_tx: tokio::sync::mpsc::Sender<DsperseSubmission>,
        rwr_tx: tokio::sync::mpsc::Sender<RwrSubmission>,
    ) -> Self {
        Self {
            relay_url,
            wallet,
            pending: Arc::new(RwLock::new(HashMap::new())),
            ws_tx: None,
            dsperse_tx,
            rwr_tx,
            dsperse_semaphore: Arc::new(Semaphore::new(DSPERSE_SEMAPHORE_CAPACITY)),
            enabled,
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        if !self.enabled {
            info!("relay disabled, skipping connection");
            return Ok(());
        }

        let url = self.relay_url.clone();
        let wallet = Arc::clone(&self.wallet);
        let pending = Arc::clone(&self.pending);
        let dsperse_tx = self.dsperse_tx.clone();
        let rwr_tx = self.rwr_tx.clone();
        let dsperse_semaphore = Arc::clone(&self.dsperse_semaphore);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Message>(256);
        self.ws_tx = Some(tx.clone());

        tokio::spawn(async move {
            let mut backoff = RELAY_RECONNECT_BASE_DELAY;
            loop {
                while rx.try_recv().is_ok() {}
                match Self::connect_and_run(
                    &url,
                    &wallet,
                    &pending,
                    &dsperse_tx,
                    &rwr_tx,
                    &dsperse_semaphore,
                    &tx,
                    &mut rx,
                )
                .await
                {
                    Ok(superseded) => {
                        if superseded {
                            info!("relay connection superseded (code 4000), exiting");
                            return;
                        }
                        info!("relay connection closed cleanly");
                        backoff = RELAY_RECONNECT_BASE_DELAY;
                        continue;
                    }
                    Err(e) => {
                        warn!(error = %e, backoff_s = backoff, "relay connection failed, reconnecting");
                    }
                }
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(RELAY_RECONNECT_MAX_DELAY);
            }
        });

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn connect_and_run(
        url: &str,
        wallet: &Wallet,
        pending: &PendingMap,
        dsperse_tx: &tokio::sync::mpsc::Sender<DsperseSubmission>,
        rwr_tx: &tokio::sync::mpsc::Sender<RwrSubmission>,
        dsperse_semaphore: &Arc<Semaphore>,
        ws_tx: &tokio::sync::mpsc::Sender<Message>,
        rx: &mut tokio::sync::mpsc::Receiver<Message>,
    ) -> Result<bool> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(url)
            .await
            .context("WebSocket connect")?;

        use futures_util::{SinkExt, StreamExt};
        let (mut write, mut read) = ws_stream.split();

        Self::authenticate(&mut read, &mut write, wallet).await?;
        info!("relay authenticated");

        let ping_interval = tokio::time::interval(Duration::from_secs(RELAY_PING_INTERVAL));
        tokio::pin!(ping_interval);
        let mut last_pong = Instant::now();

        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            match serde_json::from_str::<serde_json::Value>(&text) {
                                Ok(json) => {
                                    Self::handle_message(
                                        &json,
                                        pending,
                                        dsperse_tx,
                                        rwr_tx,
                                        dsperse_semaphore,
                                        ws_tx,
                                    ).await;
                                }
                                Err(e) => {
                                    warn!(error = %e, "relay message JSON parse failed");
                                }
                            }
                        }
                        Some(Ok(Message::Pong(_))) => {
                            last_pong = Instant::now();
                        }
                        Some(Ok(Message::Close(frame))) => {
                            let code = frame.as_ref().map(|f| f.code);
                            if code == Some(tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::from(4000)) {
                                return Ok(true);
                            }
                            return Ok(false);
                        }
                        None => return Ok(false),
                        Some(Err(e)) => return Err(e.into()),
                        _ => {}
                    }
                }
                Some(msg) = rx.recv() => {
                    write.send(msg).await?;
                }
                _ = ping_interval.tick() => {
                    if last_pong.elapsed() > Duration::from_secs(RELAY_PING_TIMEOUT) {
                        warn!("relay pong timeout after {}s, reconnecting", RELAY_PING_TIMEOUT);
                        return Err(anyhow::anyhow!("relay pong timeout"));
                    }
                    write.send(Message::Ping(vec![])).await?;
                }
            }
        }
    }

    async fn authenticate<S, R>(read: &mut R, write: &mut S, wallet: &Wallet) -> Result<()>
    where
        S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
        R: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        use futures_util::{SinkExt, StreamExt};

        let raw = tokio::time::timeout(Duration::from_secs(RELAY_AUTH_TIMEOUT), read.next())
            .await
            .context("auth challenge timeout")?
            .context("stream ended before auth")?
            .context("ws read error")?;

        let text = match raw {
            Message::Text(t) => t,
            other => anyhow::bail!("expected text for auth challenge, got {:?}", other),
        };

        let msg: serde_json::Value = serde_json::from_str(&text).context("parsing auth msg")?;
        if msg.get("type").and_then(|v| v.as_str()) != Some("auth_challenge") {
            anyhow::bail!("expected auth_challenge, got {}", text);
        }

        let challenge_b64 = msg
            .get("challenge")
            .and_then(|v| v.as_str())
            .context("missing challenge field")?;

        let challenge_bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, challenge_b64)
                .context("decoding challenge base64")?;

        let sig_bytes = wallet.sign_hotkey(&challenge_bytes)?;

        let response = serde_json::json!({
            "type": "auth_response",
            "ss58": wallet.hotkey_ss58(),
            "signature": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &sig_bytes),
        });

        write
            .send(Message::Text(serde_json::to_string(&response)?))
            .await
            .context("sending auth response")?;

        let confirm = tokio::time::timeout(Duration::from_secs(RELAY_AUTH_TIMEOUT), read.next())
            .await
            .context("auth confirm timeout")?
            .context("stream ended before confirm")?
            .context("ws read error on confirm")?;

        let confirm_text = match confirm {
            Message::Text(t) => t,
            other => anyhow::bail!("expected text for auth confirm, got {:?}", other),
        };

        let confirm_msg: serde_json::Value =
            serde_json::from_str(&confirm_text).context("parsing auth confirm")?;
        if confirm_msg.get("type").and_then(|v| v.as_str()) != Some("auth_success") {
            anyhow::bail!("auth failed: {}", confirm_text);
        }

        Ok(())
    }

    async fn handle_message(
        json: &serde_json::Value,
        pending: &PendingMap,
        dsperse_tx: &tokio::sync::mpsc::Sender<DsperseSubmission>,
        rwr_tx: &tokio::sync::mpsc::Sender<RwrSubmission>,
        dsperse_semaphore: &Arc<Semaphore>,
        ws_tx: &tokio::sync::mpsc::Sender<Message>,
    ) {
        let method = json.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let request_id = json.get("id").and_then(|v| v.as_str()).map(String::from);

        match method {
            "subnet-2.dsperse_submit" => {
                let permit = match dsperse_semaphore.try_acquire() {
                    Ok(p) => p,
                    Err(_) => {
                        Self::send_jsonrpc_error(
                            ws_tx,
                            request_id.as_deref(),
                            20,
                            "Server busy, try again later",
                        )
                        .await;
                        return;
                    }
                };

                let params = match json.get("params") {
                    Some(p) => p,
                    None => {
                        drop(permit);
                        Self::send_jsonrpc_error(
                            ws_tx,
                            request_id.as_deref(),
                            -32602,
                            "Missing params",
                        )
                        .await;
                        return;
                    }
                };

                let circuit_id = match params.get("circuit_id").and_then(|v| v.as_str()) {
                    Some(id) => id.to_string(),
                    None => {
                        drop(permit);
                        Self::send_jsonrpc_error(
                            ws_tx,
                            request_id.as_deref(),
                            -32602,
                            "Missing circuit_id",
                        )
                        .await;
                        return;
                    }
                };

                let inputs = match params.get("inputs") {
                    Some(i) => i.clone(),
                    None => {
                        drop(permit);
                        Self::send_jsonrpc_error(
                            ws_tx,
                            request_id.as_deref(),
                            -32602,
                            "Missing inputs",
                        )
                        .await;
                        return;
                    }
                };

                let req_id_for_err = request_id.clone();
                let submission = DsperseSubmission {
                    circuit_id,
                    inputs,
                    request_id,
                };

                if let Err(e) = dsperse_tx.try_send(submission) {
                    warn!(error = %e, "dsperse submission channel full or closed");
                    Self::send_jsonrpc_error(
                        ws_tx,
                        req_id_for_err.as_deref(),
                        20,
                        "Server busy, try again later",
                    )
                    .await;
                }
                drop(permit);
            }

            "subnet-2.proof_of_computation" => {
                let params = match json.get("params") {
                    Some(p) => p,
                    None => {
                        Self::send_jsonrpc_error(
                            ws_tx,
                            request_id.as_deref(),
                            -32602,
                            "Missing params",
                        )
                        .await;
                        return;
                    }
                };

                let circuit_id = match params.get("circuit").and_then(|v| v.as_str()) {
                    Some(id) => id.to_string(),
                    None => {
                        Self::send_jsonrpc_error(
                            ws_tx,
                            request_id.as_deref(),
                            -32602,
                            "Missing circuit",
                        )
                        .await;
                        return;
                    }
                };

                let inputs = match params.get("input") {
                    Some(i) => i.clone(),
                    None => {
                        Self::send_jsonrpc_error(
                            ws_tx,
                            request_id.as_deref(),
                            -32602,
                            "Missing input",
                        )
                        .await;
                        return;
                    }
                };

                let submission = RwrSubmission {
                    circuit_id,
                    inputs,
                    request_id: request_id.clone(),
                    retry_count: 0,
                };

                if let Err(e) = rwr_tx.try_send(submission) {
                    warn!(error = %e, "RWR submission channel full or closed");
                    Self::send_jsonrpc_error(
                        ws_tx,
                        request_id.as_deref(),
                        20,
                        "Server busy, try again later",
                    )
                    .await;
                }
            }

            "subnet-2.run_status" => {
                let params = match json.get("params") {
                    Some(p) => p,
                    None => {
                        Self::send_jsonrpc_error(
                            ws_tx,
                            request_id.as_deref(),
                            -32602,
                            "Missing params",
                        )
                        .await;
                        return;
                    }
                };

                let run_uid = match params.get("run_uid").and_then(|v| v.as_str()) {
                    Some(id) => id.to_string(),
                    None => {
                        Self::send_jsonrpc_error(
                            ws_tx,
                            request_id.as_deref(),
                            -32602,
                            "Missing run_uid",
                        )
                        .await;
                        return;
                    }
                };

                let mut pending_map = pending.write().await;
                if let Some(req) = pending_map.get(&run_uid) {
                    let lock = req.lock().await;
                    if let Some(result) = &lock.result {
                        let result = result.clone();
                        drop(lock);
                        pending_map.remove(&run_uid);
                        drop(pending_map);
                        Self::send_jsonrpc_result(ws_tx, request_id.as_deref(), result).await;
                    } else {
                        drop(lock);
                        drop(pending_map);
                        Self::send_jsonrpc_result(
                            ws_tx,
                            request_id.as_deref(),
                            serde_json::json!({"run_uid": run_uid, "status": "processing"}),
                        )
                        .await;
                    }
                } else {
                    drop(pending_map);
                    Self::send_jsonrpc_error(ws_tx, request_id.as_deref(), 11, "Run not found")
                        .await;
                }
            }

            _ => {
                if let Some(id) = json.get("id").and_then(|v| v.as_str()) {
                    let map = pending.read().await;
                    if let Some(req) = map.get(id) {
                        let mut lock = req.lock().await;
                        lock.result = Some(json.clone());
                        lock.notify.notify_one();
                    }
                }
            }
        }
    }

    pub async fn set_request_result(&self, request_hash: &str, result: serde_json::Value) {
        let map = self.pending.read().await;
        if let Some(req) = map.get(request_hash) {
            let mut lock = req.lock().await;
            lock.result = Some(result);
            lock.notify.notify_one();
        }
    }

    pub async fn register_pending(&self, hash: &str) -> Arc<Mutex<PendingRequest>> {
        let entry = Arc::new(Mutex::new(PendingRequest {
            result: None,
            notify: Arc::new(Notify::new()),
        }));
        let mut map = self.pending.write().await;
        map.insert(hash.to_string(), Arc::clone(&entry));
        entry
    }

    pub async fn remove_pending(&self, hash: &str) {
        let mut map = self.pending.write().await;
        map.remove(hash);
    }

    pub async fn send_response(&self, request_id: &str, result: serde_json::Value) {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "result": result,
            "id": request_id,
        });
        if let Some(tx) = &self.ws_tx {
            let text = serde_json::to_string(&msg).unwrap_or_default();
            let _ = tx.try_send(Message::Text(text));
        }
    }

    pub async fn send_notification(&self, method: &str, params: serde_json::Value) {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        if let Some(tx) = &self.ws_tx {
            let text = serde_json::to_string(&msg).unwrap_or_default();
            let _ = tx.try_send(Message::Text(text));
        }
    }

    async fn send_jsonrpc_error(
        ws_tx: &tokio::sync::mpsc::Sender<Message>,
        request_id: Option<&str>,
        code: i32,
        message: &str,
    ) {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "error": {"code": code, "message": message},
            "id": request_id,
        });
        let text = serde_json::to_string(&msg).unwrap_or_default();
        let _ = ws_tx.try_send(Message::Text(text));
    }

    async fn send_jsonrpc_result(
        ws_tx: &tokio::sync::mpsc::Sender<Message>,
        request_id: Option<&str>,
        result: serde_json::Value,
    ) {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "result": result,
            "id": request_id,
        });
        let text = serde_json::to_string(&msg).unwrap_or_default();
        let _ = ws_tx.try_send(Message::Text(text));
    }
}
