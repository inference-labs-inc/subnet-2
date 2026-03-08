use std::sync::Arc;

use anyhow::{Context, Result};
use std::path::Path;
use tokio::net::UnixListener;
use tracing::{error, info, warn};

use sn2_verify::protocol::{EvictResponse, ReconstructResponse, ServiceRequest};
use sn2_verify::store::TileStore;
use sn2_verify::{codec, protocol, store, verify};

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
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _addr)) => {
                        let store = Arc::clone(&store);
                        tokio::spawn(handle_connection(stream, store));
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

async fn handle_connection(stream: tokio::net::UnixStream, store: Arc<TileStore>) {
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
