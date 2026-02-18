mod codec;
mod expander;
mod field;
mod protocol;
mod verify;
mod witness;

use anyhow::{Context, Result};
use std::path::Path;
use tokio::net::UnixListener;
use tracing::{error, info};

const SOCKET_PATH: &str = "/tmp/sn2-verify.sock";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let sock_path = std::env::var("SN2_VERIFY_SOCK")
        .unwrap_or_else(|_| SOCKET_PATH.to_string());

    if Path::new(&sock_path).exists() {
        std::fs::remove_file(&sock_path).context("removing stale socket")?;
    }

    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding unix socket at {sock_path}"))?;
    info!(path = %sock_path, "sn2-verify listening");

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _addr)) => {
                        tokio::spawn(handle_connection(stream));
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

async fn handle_connection(stream: tokio::net::UnixStream) {
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

        let req: protocol::VerifyRequest = match rmp_serde::from_slice(&frame) {
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

        let request_id = req.request_id.clone();
        info!(request_id = %request_id, "processing verification request");

        let resp = verify::handle_request(req).await;

        match rmp_serde::to_vec_named(&resp) {
            Ok(data) => {
                if let Err(e) = codec::write_frame(&mut writer, &data).await {
                    error!(request_id = %request_id, error = %e, "write error");
                    return;
                }
            }
            Err(e) => {
                error!(request_id = %request_id, error = %e, "serialize error");
                return;
            }
        }
    }
}
