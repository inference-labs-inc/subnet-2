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

const MSG_AUTH_CHALLENGE: u8 = 0x01;
const MSG_AUTH_RESPONSE: u8 = 0x02;
const MSG_AUTH_SUCCESS: u8 = 0x03;
const MSG_SUBMIT: u8 = 0x10;
const MSG_SUBMIT_RESULT: u8 = 0x11;
const MSG_STATUS_REQ: u8 = 0x20;
const MSG_STATUS_RESULT: u8 = 0x21;
const MSG_PROOF_REQ: u8 = 0x30;
const MSG_PROOF_RESULT: u8 = 0x31;
const MSG_BATCH_DONE: u8 = 0x40;
const MSG_ERROR: u8 = 0xFE;

pub const FRAME_SUBMIT_RESULT: u8 = MSG_SUBMIT_RESULT;
pub const FRAME_PROOF_RESULT: u8 = MSG_PROOF_RESULT;

fn decode_frame(data: &[u8]) -> Result<(u8, u32, &[u8])> {
    anyhow::ensure!(data.len() >= 5, "frame too short");
    let msg_type = data[0];
    let req_id = u32::from_be_bytes([data[1], data[2], data[3], data[4]]);
    Ok((msg_type, req_id, &data[5..]))
}

fn encode_frame(msg_type: u8, req_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(msg_type);
    buf.extend_from_slice(&req_id.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

fn decode_submit_payload(payload: &[u8]) -> Result<(serde_json::Value, Option<&[u8]>)> {
    anyhow::ensure!(payload.len() >= 4, "submit payload too short");
    let meta_len = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    let meta_end = 4usize
        .checked_add(meta_len)
        .context("submit payload meta length overflow")?;
    anyhow::ensure!(payload.len() >= meta_end, "submit payload meta truncated");
    let meta: serde_json::Value =
        serde_json::from_slice(&payload[4..meta_end]).context("parsing submit meta")?;
    let tensor = if payload.len() > meta_end {
        Some(&payload[meta_end..])
    } else {
        None
    };
    Ok((meta, tensor))
}

type PendingMap = Arc<RwLock<HashMap<String, Arc<Mutex<PendingRequest>>>>>;

pub(crate) struct PendingRequest {
    result: Option<serde_json::Value>,
    notify: Arc<Notify>,
}

#[derive(Debug)]
pub struct DsperseSubmission {
    pub circuit_id: String,
    pub inputs: serde_json::Value,
    pub tensor_data: Option<Vec<u8>>,
    pub request_id: Option<u32>,
    #[allow(dead_code)]
    pub permit: tokio::sync::OwnedSemaphorePermit,
}

#[derive(Debug, Clone)]
pub struct RwrSubmission {
    pub circuit_id: String,
    pub inputs: serde_json::Value,
    pub request_id: Option<u32>,
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
        use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

        let config = WebSocketConfig {
            max_message_size: Some(128 * 1024 * 1024),
            max_frame_size: Some(128 * 1024 * 1024),
            ..Default::default()
        };
        let (ws_stream, _) = tokio_tungstenite::connect_async_with_config(url, Some(config), false)
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
                        Some(Ok(Message::Binary(data))) => {
                            match decode_frame(&data) {
                                Ok((msg_type, req_id, payload)) => {
                                    Self::handle_binary_message(
                                        msg_type,
                                        req_id,
                                        payload,
                                        pending,
                                        dsperse_tx,
                                        rwr_tx,
                                        dsperse_semaphore,
                                        ws_tx,
                                    ).await;
                                }
                                Err(e) => {
                                    warn!(error = %e, "invalid binary frame from relay");
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

        let data = match raw {
            Message::Binary(d) => d,
            other => anyhow::bail!("expected binary for auth challenge, got {:?}", other),
        };

        let (msg_type, _req_id, payload) =
            decode_frame(&data).context("decoding auth challenge frame")?;
        anyhow::ensure!(
            msg_type == MSG_AUTH_CHALLENGE,
            "expected MSG_AUTH_CHALLENGE (0x01), got 0x{msg_type:02x}"
        );

        let msg: serde_json::Value =
            serde_json::from_slice(payload).context("parsing auth challenge JSON")?;
        let challenge_b64 = msg
            .get("challenge")
            .and_then(|v| v.as_str())
            .context("missing challenge field")?;

        let challenge_bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, challenge_b64)
                .context("decoding challenge base64")?;

        let sig_bytes = wallet.sign_hotkey(&challenge_bytes)?;

        let response_json = serde_json::json!({
            "ss58": wallet.hotkey_ss58(),
            "signature": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &sig_bytes),
        });
        let response_payload = serde_json::to_vec(&response_json)?;
        let response_frame = encode_frame(MSG_AUTH_RESPONSE, 0, &response_payload);

        write
            .send(Message::Binary(response_frame))
            .await
            .context("sending auth response")?;

        let confirm = tokio::time::timeout(Duration::from_secs(RELAY_AUTH_TIMEOUT), read.next())
            .await
            .context("auth confirm timeout")?
            .context("stream ended before confirm")?
            .context("ws read error on confirm")?;

        let confirm_data = match confirm {
            Message::Binary(d) => d,
            other => anyhow::bail!("expected binary for auth confirm, got {:?}", other),
        };

        let (confirm_type, _, _) =
            decode_frame(&confirm_data).context("decoding auth confirm frame")?;
        if confirm_type != MSG_AUTH_SUCCESS {
            anyhow::bail!(
                "auth failed: expected MSG_AUTH_SUCCESS, got 0x{:02x}",
                confirm_type
            );
        }

        Ok(())
    }

    fn parse_circuit_payload(meta: &serde_json::Value) -> Option<(String, serde_json::Value)> {
        let circuit_id = meta.get("circuit_id").and_then(|v| v.as_str())?.to_string();
        let inputs = meta
            .get("inputs")
            .cloned()
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
        Some((circuit_id, inputs))
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_binary_message(
        msg_type: u8,
        req_id: u32,
        payload: &[u8],
        pending: &PendingMap,
        dsperse_tx: &tokio::sync::mpsc::Sender<DsperseSubmission>,
        rwr_tx: &tokio::sync::mpsc::Sender<RwrSubmission>,
        dsperse_semaphore: &Arc<Semaphore>,
        ws_tx: &tokio::sync::mpsc::Sender<Message>,
    ) {
        match msg_type {
            MSG_SUBMIT => {
                Self::handle_submit(req_id, payload, dsperse_tx, dsperse_semaphore, ws_tx).await;
            }
            MSG_PROOF_REQ => {
                Self::handle_proof_req(req_id, payload, rwr_tx, ws_tx).await;
            }
            MSG_STATUS_REQ => {
                Self::handle_status_req(req_id, payload, pending, ws_tx).await;
            }
            _ => {
                warn!(
                    msg_type = msg_type,
                    "unknown binary message type from relay"
                );
            }
        }
    }

    async fn handle_submit(
        req_id: u32,
        payload: &[u8],
        dsperse_tx: &tokio::sync::mpsc::Sender<DsperseSubmission>,
        dsperse_semaphore: &Arc<Semaphore>,
        ws_tx: &tokio::sync::mpsc::Sender<Message>,
    ) {
        let permit = match Arc::clone(dsperse_semaphore).try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                Self::send_error(ws_tx, req_id, 20, "Server busy, try again later").await;
                return;
            }
        };

        let (meta, tensor_data) = match decode_submit_payload(payload) {
            Ok(v) => v,
            Err(e) => {
                Self::send_error(ws_tx, req_id, -32602, &format!("Invalid payload: {e}")).await;
                return;
            }
        };

        let (circuit_id, inputs) = match Self::parse_circuit_payload(&meta) {
            Some(v) => v,
            None => {
                Self::send_error(ws_tx, req_id, -32602, "Missing circuit_id").await;
                return;
            }
        };

        let req_id_opt = if req_id > 0 { Some(req_id) } else { None };

        let submission = DsperseSubmission {
            circuit_id,
            inputs,
            tensor_data: tensor_data.map(|d| d.to_vec()),
            request_id: req_id_opt,
            permit,
        };

        if let Err(e) = dsperse_tx.try_send(submission) {
            warn!(error = %e, "dsperse submission channel full or closed");
            Self::send_error(ws_tx, req_id, 20, "Server busy, try again later").await;
        }
    }

    async fn handle_proof_req(
        req_id: u32,
        payload: &[u8],
        rwr_tx: &tokio::sync::mpsc::Sender<RwrSubmission>,
        ws_tx: &tokio::sync::mpsc::Sender<Message>,
    ) {
        let meta: serde_json::Value = match serde_json::from_slice(payload) {
            Ok(v) => v,
            Err(e) => {
                Self::send_error(
                    ws_tx,
                    req_id,
                    -32602,
                    &format!("Invalid proof payload: {e}"),
                )
                .await;
                return;
            }
        };

        let (circuit_id, inputs) = match Self::parse_circuit_payload(&meta) {
            Some(v) => v,
            None => {
                Self::send_error(ws_tx, req_id, -32602, "Missing circuit_id").await;
                return;
            }
        };

        let req_id_opt = if req_id > 0 { Some(req_id) } else { None };

        let submission = RwrSubmission {
            circuit_id,
            inputs,
            request_id: req_id_opt,
            retry_count: 0,
        };

        if let Err(e) = rwr_tx.try_send(submission) {
            warn!(error = %e, "RWR submission channel full or closed");
            Self::send_error(ws_tx, req_id, 20, "Server busy, try again later").await;
        }
    }

    async fn handle_status_req(
        req_id: u32,
        payload: &[u8],
        pending: &PendingMap,
        ws_tx: &tokio::sync::mpsc::Sender<Message>,
    ) {
        let meta: serde_json::Value = match serde_json::from_slice(payload) {
            Ok(v) => v,
            Err(e) => {
                Self::send_error(
                    ws_tx,
                    req_id,
                    -32602,
                    &format!("Invalid status payload: {e}"),
                )
                .await;
                return;
            }
        };

        let run_uid = match meta.get("run_uid").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                Self::send_error(ws_tx, req_id, -32602, "Missing run_uid").await;
                return;
            }
        };

        let pending_map = pending.read().await;
        if let Some(req) = pending_map.get(&run_uid) {
            let lock = req.lock().await;
            if let Some(result) = &lock.result {
                let result = result.clone();
                drop(lock);
                drop(pending_map);
                if Self::send_result(ws_tx, MSG_STATUS_RESULT, req_id, result).await {
                    let mut pending_map = pending.write().await;
                    pending_map.remove(&run_uid);
                } else {
                    warn!(run_uid = %run_uid, "status result send failed, retaining pending entry");
                }
            } else {
                drop(lock);
                drop(pending_map);
                Self::send_result(
                    ws_tx,
                    MSG_STATUS_RESULT,
                    req_id,
                    serde_json::json!({"run_uid": run_uid, "status": "processing"}),
                )
                .await;
            }
        } else {
            drop(pending_map);
            Self::send_error(ws_tx, req_id, 11, "Run not found").await;
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

    pub async fn send_response(&self, msg_type: u8, req_id: u32, result: serde_json::Value) {
        if let Some(tx) = &self.ws_tx {
            let payload = serde_json::to_vec(&result).unwrap_or_default();
            let frame = encode_frame(msg_type, req_id, &payload);
            let _ = tx.try_send(Message::Binary(frame));
        }
    }

    pub async fn send_notification(&self, method: &str, params: serde_json::Value) {
        if let Some(tx) = &self.ws_tx {
            let msg_type = match method {
                "subnet-2.batch_completed" => MSG_BATCH_DONE,
                _ => {
                    warn!(method = method, "unknown notification method");
                    return;
                }
            };
            let payload = serde_json::to_vec(&params).unwrap_or_default();
            let frame = encode_frame(msg_type, 0, &payload);
            let _ = tx.try_send(Message::Binary(frame));
        }
    }

    async fn send_error(
        ws_tx: &tokio::sync::mpsc::Sender<Message>,
        req_id: u32,
        code: i32,
        message: &str,
    ) {
        let payload = serde_json::to_vec(&serde_json::json!({
            "code": code,
            "message": message,
        }))
        .unwrap_or_default();
        let frame = encode_frame(MSG_ERROR, req_id, &payload);
        let _ = ws_tx.try_send(Message::Binary(frame));
    }

    async fn send_result(
        ws_tx: &tokio::sync::mpsc::Sender<Message>,
        msg_type: u8,
        req_id: u32,
        result: serde_json::Value,
    ) -> bool {
        let payload = serde_json::to_vec(&result).unwrap_or_default();
        let frame = encode_frame(msg_type, req_id, &payload);
        ws_tx.try_send(Message::Binary(frame)).is_ok()
    }
}
