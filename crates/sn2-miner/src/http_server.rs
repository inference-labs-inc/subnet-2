use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Json, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use sn2_chain::Metagraph;
use sn2_types::*;

use crate::handlers::MinerHandlers;
use crate::signature;

struct AppState {
    handlers: Arc<MinerHandlers>,
    miner_hotkey: String,
    metagraph: Arc<RwLock<Metagraph>>,
    disable_blacklist: bool,
}

pub async fn run_http_server(
    host: &str,
    port: u16,
    handlers: Arc<MinerHandlers>,
    miner_hotkey: &str,
    metagraph: Arc<RwLock<Metagraph>>,
    disable_blacklist: bool,
) -> Result<()> {
    let state = Arc::new(AppState {
        handlers,
        miner_hotkey: miner_hotkey.to_string(),
        metagraph,
        disable_blacklist,
    });

    let synapse_routes = Router::new()
        .route(
            &format!("/{}", QueryZkProof::NAME),
            post(handle_query_zk_proof),
        )
        .route(
            &format!("/{}", ProofOfWeightsDataModel::NAME),
            post(handle_proof_of_weights),
        )
        .route(&format!("/{}", Competition::NAME), post(handle_competition))
        .route(
            &format!("/{}", DSliceProofGenerationDataModel::NAME),
            post(handle_dslice),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            verify_signature_middleware,
        ))
        .with_state(state.clone());

    let app = Router::new()
        .merge(synapse_routes)
        .route("/health", axum::routing::get(health))
        .with_state(state);

    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!(addr = %addr, "HTTP server listening");

    axum::serve(listener, app).await?;
    Ok(())
}

async fn verify_signature_middleware(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let validator_hotkey = headers
        .get("validator-hotkey")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let sig = headers
        .get("signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let nonce = headers
        .get("nonce")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let miner_hotkey = headers
        .get("miner-hotkey")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !miner_hotkey.is_empty() && miner_hotkey != state.miner_hotkey {
        warn!(expected = %state.miner_hotkey, got = miner_hotkey, "miner hotkey mismatch");
        return Err(StatusCode::UNAUTHORIZED);
    }

    let (parts, body) = request.into_parts();
    let body_bytes = axum::body::to_bytes(body, 10 * 1024 * 1024)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    if validator_hotkey.is_empty() || sig.is_empty() || nonce.is_empty() {
        warn!("missing auth headers (validator-hotkey, signature, or nonce)");
        return Err(StatusCode::UNAUTHORIZED);
    }

    let payload_hash = hex::encode(Sha256::digest(&body_bytes));

    match signature::verify_request_signature(nonce, validator_hotkey, &payload_hash, sig) {
        Ok(true) => {}
        Ok(false) => {
            warn!(validator = validator_hotkey, "invalid signature");
            return Err(StatusCode::UNAUTHORIZED);
        }
        Err(e) => {
            warn!(validator = validator_hotkey, error = %e, "signature verification error");
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    if !state.disable_blacklist {
        let meta = state.metagraph.read().await;
        match meta.get_uid_by_hotkey(validator_hotkey) {
            Some(uid) => {
                let neuron = meta.get_neuron(uid);
                let has_permit = neuron.map(|n| n.validator_permit).unwrap_or(false);
                let stake = neuron.map(|n| n.stake).unwrap_or(0);
                debug!(
                    validator = validator_hotkey,
                    uid = uid,
                    stake = stake,
                    permit = has_permit,
                    "request from validator"
                );
                if !has_permit {
                    warn!(
                        validator = validator_hotkey,
                        uid = uid,
                        "no validator permit"
                    );
                    return Err(StatusCode::FORBIDDEN);
                }
            }
            None => {
                warn!(
                    validator = validator_hotkey,
                    "hotkey not registered in metagraph"
                );
                return Err(StatusCode::FORBIDDEN);
            }
        }
    }

    let request = Request::from_parts(parts, axum::body::Body::from(body_bytes));
    Ok(next.run(request).await)
}

async fn health() -> &'static str {
    "ok"
}

async fn handle_query_zk_proof(
    State(state): State<Arc<AppState>>,
    Json(data): Json<QueryZkProof>,
) -> impl IntoResponse {
    match state.handlers.handle_query_zk_proof(data).await {
        Ok(result) => (StatusCode::OK, Json(result)),
        Err(e) => {
            error!(error = %e, "QueryZkProof handler");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        }
    }
}

async fn handle_proof_of_weights(
    State(state): State<Arc<AppState>>,
    Json(data): Json<ProofOfWeightsDataModel>,
) -> impl IntoResponse {
    match state.handlers.handle_proof_of_weights(data).await {
        Ok(result) => (StatusCode::OK, Json(result)),
        Err(e) => {
            error!(error = %e, "ProofOfWeights handler");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        }
    }
}

async fn handle_competition(
    State(state): State<Arc<AppState>>,
    Json(data): Json<Competition>,
) -> impl IntoResponse {
    match state.handlers.handle_competition(data).await {
        Ok(result) => (StatusCode::OK, Json(result)),
        Err(e) => {
            error!(error = %e, "Competition handler");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        }
    }
}

async fn handle_dslice(
    State(state): State<Arc<AppState>>,
    Json(data): Json<DSliceProofGenerationDataModel>,
) -> impl IntoResponse {
    match state.handlers.handle_dslice(data).await {
        Ok(result) => (StatusCode::OK, Json(result)),
        Err(e) => {
            error!(error = %e, "DSlice handler");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        }
    }
}
