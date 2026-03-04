use std::sync::Arc;

use anyhow::{Context, Result};
use std::path::Path;
use tokio::net::UnixListener;
use tracing::{error, info, warn};

use sn2_verify::protocol::{
    EvictResponse, QueryVerifyStoreRequest, QueryVerifyStoreResponse, ReconstructResponse,
    ServiceRequest,
};
use sn2_verify::store::{StoredTile, TileStore};
use sn2_verify::{codec, http_client, miner_response, protocol, store, verify};

const SOCKET_PATH: &str = "/tmp/sn2-verify.sock";

#[tokio::main]
async fn main() -> Result<()> {
    sn2_types::init_tracing("info");

    let sock_path = std::env::var("SN2_VERIFY_SOCK").unwrap_or_else(|_| SOCKET_PATH.to_string());

    if Path::new(&sock_path).exists() {
        std::fs::remove_file(&sock_path).context("removing stale socket")?;
    }

    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding unix socket at {sock_path}"))?;
    info!(path = %sock_path, "sn2-verify listening");

    let store = Arc::new(TileStore::new());
    let http_client = Arc::new(
        reqwest::Client::builder()
            .pool_max_idle_per_host(32)
            .tcp_nodelay(true)
            .build()
            .context("creating HTTP client")?,
    );
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _addr)) => {
                        let store = Arc::clone(&store);
                        let http_client = Arc::clone(&http_client);
                        tokio::spawn(handle_connection(stream, store, http_client));
                    }
                    Err(e) => {
                        error!(error = %e, "accept failed");
                    }
                }
            }
            _ = &mut shutdown => {
                info!("shutting down");
                break;
            }
        }
    }

    let _ = std::fs::remove_file(&sock_path);
    Ok(())
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    store: Arc<TileStore>,
    http_client: Arc<reqwest::Client>,
) {
    let (mut reader, mut writer) = stream.into_split();
    let peer = "unix-client";
    info!(peer, "connection accepted");

    loop {
        let frame = match codec::read_frame(&mut reader).await {
            Ok(Some(f)) => f,
            Ok(None) => {
                info!(peer, "connection closed");
                return;
            }
            Err(e) => {
                error!(peer, error = %e, "read error");
                return;
            }
        };

        let req: ServiceRequest = match rmp_serde::from_slice(&frame) {
            Ok(r) => r,
            Err(e) => {
                error!(peer, error = %e, "deserialize error");
                let resp = protocol::VerifyResponse::error(
                    String::new(),
                    format!("deserialize error: {e}"),
                );
                if let Ok(data) = rmp_serde::to_vec_named(&resp) {
                    let _ = codec::write_frame(&mut writer, &data).await;
                }
                continue;
            }
        };

        let response_data = match req {
            ServiceRequest::Verify(vr) => {
                let request_id = vr.request_id.clone();
                info!(request_id = %request_id, "processing verify request");
                let resp = verify::handle_request(vr).await;
                rmp_serde::to_vec_named(&resp)
            }
            ServiceRequest::VerifyAndStore(vsr) => {
                let request_id = vsr.request_id.clone();
                let store_key = vsr.store_key.clone();
                info!(request_id = %request_id, store_key = %store_key, "processing verify_and_store request");
                let resp = verify::handle_store_request(vsr, &store).await;
                rmp_serde::to_vec_named(&resp)
            }
            ServiceRequest::Store(sr) => {
                let key = sr.store_key.clone();
                info!(store_key = %key, len = sr.data.len(), "processing store request");
                let resp = match store.insert(
                    sr.store_key,
                    store::StoredTile {
                        data: sr.data,
                        channels: sr.channels,
                        height: sr.height,
                        width: sr.width,
                    },
                ) {
                    Ok(()) => protocol::StoreResponse::ok(String::new()),
                    Err(e) => {
                        warn!(error = %e, "tile store insert failed");
                        protocol::StoreResponse::error(String::new(), format!("{e:#}"))
                    }
                };
                rmp_serde::to_vec_named(&resp)
            }
            ServiceRequest::Reconstruct(rr) => {
                info!(
                    tile_count = rr.tile_keys.len(),
                    tiles_y = rr.tiles_y,
                    tiles_x = rr.tiles_x,
                    "processing reconstruct request"
                );
                let resp = match store.reconstruct(&rr.tile_keys, rr.tiles_y, rr.tiles_x) {
                    Ok(output) => ReconstructResponse {
                        success: true,
                        output: Some(output),
                        error: None,
                    },
                    Err(e) => {
                        warn!(error = %e, "reconstruct failed");
                        ReconstructResponse {
                            success: false,
                            output: None,
                            error: Some(format!("{e:#}")),
                        }
                    }
                };
                rmp_serde::to_vec_named(&resp)
            }
            ServiceRequest::Evict(er) => {
                let resp = match store.evict(&er.keys) {
                    Ok(evicted) => EvictResponse {
                        success: true,
                        evicted,
                    },
                    Err(e) => {
                        warn!(error = %e, "tile store evict failed");
                        EvictResponse {
                            success: false,
                            evicted: 0,
                        }
                    }
                };
                rmp_serde::to_vec_named(&resp)
            }
            ServiceRequest::QueryVerifyStore(qvs) => {
                let request_id = qvs.request_id.clone();
                let miner = format!("{}:{}", qvs.miner_ip, qvs.miner_port);
                info!(request_id = %request_id, miner = %miner, "processing query_verify_store");
                let resp = handle_query_verify_store(qvs, &http_client, &store).await;
                info!(
                    request_id = %request_id,
                    success = resp.success,
                    verification = resp.verification_result,
                    http_status = resp.http_status,
                    elapsed = format!("{:.3}s", resp.response_time_secs),
                    "query_verify_store complete"
                );
                rmp_serde::to_vec_named(&resp)
            }
        };

        match response_data {
            Ok(data) => {
                if let Err(e) = codec::write_frame(&mut writer, &data).await {
                    error!(error = %e, "write error");
                    return;
                }
            }
            Err(e) => {
                error!(error = %e, "serialize error");
                return;
            }
        }
    }
}

async fn handle_query_verify_store(
    req: QueryVerifyStoreRequest,
    http_client: &reqwest::Client,
    store: &Arc<TileStore>,
) -> QueryVerifyStoreResponse {
    let http_result = http_client::query_miner_http(
        http_client,
        &req.miner_ip,
        req.miner_port,
        &req.url_path,
        &req.request_body_json,
        &req.headers,
        req.timeout_secs,
    )
    .await;

    let miner_resp = match http_result {
        Ok(r) => r,
        Err(e) => {
            return QueryVerifyStoreResponse::http_error(req.request_id, format!("{e:#}"));
        }
    };

    let body_bytes = miner_resp.body_bytes;
    let http_status = miner_resp.status;
    let http_elapsed = miner_resp.elapsed_secs;

    let body: serde_json::Value =
        match tokio::task::spawn_blocking(move || serde_json::from_slice(&body_bytes)).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                return QueryVerifyStoreResponse::miner_error(
                    req.request_id,
                    http_status,
                    http_elapsed,
                    format!("JSON parse error: {e:#}"),
                );
            }
            Err(e) => {
                return QueryVerifyStoreResponse::miner_error(
                    req.request_id,
                    http_status,
                    http_elapsed,
                    format!("JSON parse task panicked: {e}"),
                );
            }
        };

    let fields = match miner_response::extract_dslice_fields(&body) {
        Ok(f) => f,
        Err(e) => {
            return QueryVerifyStoreResponse::miner_error(
                req.request_id,
                http_status,
                http_elapsed,
                format!("{e:#}"),
            );
        }
    };

    let witness_hex = match fields.witness_hex {
        Some(w) => w,
        None => {
            return QueryVerifyStoreResponse {
                request_id: req.request_id,
                success: false,
                http_status,
                response_time_secs: http_elapsed,
                verification_result: false,
                proof_size: fields.proof_size,
                stored: false,
                is_incremental: false,
                error: Some("no witness in miner response".to_string()),
            };
        }
    };

    let verify_result = verify::verify_inner(
        &req.request_id,
        &req.circuit_path,
        &witness_hex,
        &fields.proof_hex,
        req.num_inputs,
        &req.expected_inputs,
        &req.pcs_type,
    )
    .await;

    match verify_result {
        Ok(vr) => {
            let (stored, store_error): (bool, Option<String>) = match (
                req.store_key,
                req.output_shape,
            ) {
                (Some(_), None) | (None, Some(_)) => {
                    warn!("partial store parameters: store_key and output_shape must both be present or both absent");
                    (false, Some("partial store parameters".to_string()))
                }
                (None, None) => (false, None),
                (Some(key), Some(shape)) => {
                    let [batch, channels, height, width] = shape;
                    if batch != 1 {
                        return QueryVerifyStoreResponse {
                            request_id: req.request_id,
                            success: true,
                            http_status,
                            response_time_secs: http_elapsed,
                            verification_result: true,
                            proof_size: fields.proof_size,
                            stored: false,
                            is_incremental: fields.is_incremental,
                            error: Some(format!("leading output dimension is {batch}, expected 1")),
                        };
                    }
                    match channels
                        .checked_mul(height)
                        .and_then(|v| v.checked_mul(width))
                    {
                        None => (
                            false,
                            Some(format!(
                                "output shape dimensions overflow: {}x{}x{}",
                                channels, height, width
                            )),
                        ),
                        Some(expected_len) => {
                            if vr.rescaled_outputs.len() == expected_len {
                                match store.insert(
                                    key,
                                    StoredTile {
                                        data: vr.rescaled_outputs,
                                        channels,
                                        height,
                                        width,
                                    },
                                ) {
                                    Ok(()) => (true, None),
                                    Err(e) => {
                                        warn!(error = %e, "tile store insert failed during query_verify_store");
                                        (false, Some(format!("{e:#}")))
                                    }
                                }
                            } else {
                                warn!(
                                    expected = expected_len,
                                    actual = vr.rescaled_outputs.len(),
                                    "output length mismatch, not storing tile"
                                );
                                (
                                    false,
                                    Some(format!(
                                        "output length {} != expected {}",
                                        vr.rescaled_outputs.len(),
                                        expected_len
                                    )),
                                )
                            }
                        }
                    }
                }
            };

            QueryVerifyStoreResponse {
                request_id: req.request_id,
                success: true,
                http_status,
                response_time_secs: http_elapsed,
                verification_result: true,
                proof_size: fields.proof_size,
                stored,
                is_incremental: fields.is_incremental,
                error: store_error,
            }
        }
        Err(e) => {
            warn!(
                request_id = %req.request_id,
                error = %e,
                "query_verify_store verification failed"
            );
            QueryVerifyStoreResponse {
                request_id: req.request_id,
                success: true,
                http_status,
                response_time_secs: http_elapsed,
                verification_result: false,
                proof_size: fields.proof_size,
                stored: false,
                is_incremental: fields.is_incremental,
                error: Some(format!("{e:#}")),
            }
        }
    }
}
