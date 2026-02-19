use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use sn2_chain::Wallet;
use tracing::{info, warn};

use crate::incremental_runner::SliceArtifact;

const DEFAULT_API_URL: &str = "https://sn2-api.inferencelabs.com";

pub struct ProofUploader {
    http: reqwest::Client,
    wallet: Arc<Wallet>,
    api_base_url: String,
}

impl ProofUploader {
    pub fn new(wallet: Arc<Wallet>, api_base_url: Option<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
            wallet,
            api_base_url: api_base_url.unwrap_or_else(|| DEFAULT_API_URL.to_string()),
        }
    }

    fn sign_body(&self, body: &[u8]) -> Result<String> {
        let sig_bytes = self.wallet.sign_hotkey(body)?;
        Ok(base64::engine::general_purpose::STANDARD.encode(&sig_bytes))
    }

    pub async fn upload_run_artifacts(
        &self,
        run_uid: &str,
        circuit_id: &str,
        circuit_name: &str,
        artifacts: Vec<SliceArtifact>,
        final_output: Option<serde_json::Value>,
    ) -> Result<()> {
        if artifacts.is_empty() {
            return Ok(());
        }

        let artifact_metas: Vec<serde_json::Value> = artifacts
            .iter()
            .map(|a| {
                serde_json::json!({
                    "slice_num": a.slice_num,
                    "artifact_type": "proof",
                })
            })
            .collect();

        let urls = self
            .request_upload_urls(run_uid, circuit_id, &artifact_metas)
            .await?;

        let mut confirmed = Vec::new();

        for (artifact, url_entry) in artifacts.iter().zip(urls.iter()) {
            let upload_url = match url_entry.get("upload_url").and_then(|v| v.as_str()) {
                Some(u) => u,
                None => continue,
            };
            let gcs_key = url_entry
                .get("gcs_key")
                .and_then(|v| v.as_str())
                .unwrap_or_default();

            let proof_bytes = match &artifact.proof_hex {
                Some(hex_str) => match hex::decode(hex_str) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        warn!(slice = %artifact.slice_num, error = %e, "hex decode failed, skipping artifact");
                        continue;
                    }
                },
                None => continue,
            };

            if let Err(e) = self.upload_artifact(upload_url, &proof_bytes).await {
                warn!(
                    slice = %artifact.slice_num,
                    error = %e,
                    "artifact upload failed, skipping"
                );
                continue;
            }

            confirmed.push(serde_json::json!({
                "slice_num": artifact.slice_num,
                "proof_system": artifact.proof_system.as_ref().map(|ps| ps.to_string()),
                "gcs_key": gcs_key,
                "size_bytes": proof_bytes.len(),
                "tile_idx": artifact.tile_idx,
                "artifact_type": "proof",
            }));
        }

        if !confirmed.is_empty() {
            self.confirm_uploads(run_uid, circuit_id, circuit_name, confirmed)
                .await?;
        }

        if let Some(output) = final_output {
            self.upload_final_output(run_uid, circuit_id, &output)
                .await?;
        }

        info!(run_uid = %run_uid, "proof artifacts uploaded");
        Ok(())
    }

    async fn request_upload_urls(
        &self,
        run_uid: &str,
        circuit_id: &str,
        artifacts: &[serde_json::Value],
    ) -> Result<Vec<serde_json::Value>> {
        let body = serde_json::json!({
            "validator_key": self.wallet.hotkey_ss58(),
            "run_uid": run_uid,
            "circuit_id": circuit_id,
            "artifacts": artifacts,
        });

        let body_bytes = serde_json::to_vec(&body)?;
        let sig = self.sign_body(&body_bytes)?;

        let resp = self
            .http
            .post(format!("{}/proofs/upload-urls", self.api_base_url))
            .header("Content-Type", "application/json")
            .header("X-Request-Signature", sig)
            .body(body_bytes)
            .send()
            .await
            .context("requesting upload URLs")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("upload-urls returned {status}: {text}");
        }

        let parsed: serde_json::Value = serde_json::from_str(&text)?;
        let urls = parsed
            .get("urls")
            .or_else(|| parsed.get("artifacts"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(urls)
    }

    async fn upload_artifact(&self, upload_url: &str, data: &[u8]) -> Result<()> {
        let resp = self
            .http
            .put(upload_url)
            .header("Content-Type", "application/octet-stream")
            .body(data.to_vec())
            .send()
            .await
            .context("uploading artifact")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("artifact PUT returned {status}: {text}");
        }
        Ok(())
    }

    async fn confirm_uploads(
        &self,
        run_uid: &str,
        circuit_id: &str,
        circuit_name: &str,
        confirmed: Vec<serde_json::Value>,
    ) -> Result<()> {
        let body = serde_json::json!({
            "validator_key": self.wallet.hotkey_ss58(),
            "run_uid": run_uid,
            "circuit_id": circuit_id,
            "circuit_name": circuit_name,
            "artifacts": confirmed,
        });

        let body_bytes = serde_json::to_vec(&body)?;
        let sig = self.sign_body(&body_bytes)?;

        let resp = self
            .http
            .post(format!("{}/proofs/confirm", self.api_base_url))
            .header("Content-Type", "application/json")
            .header("X-Request-Signature", sig)
            .body(body_bytes)
            .send()
            .await
            .context("confirming uploads")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("confirm_uploads returned {status}: {text}");
        }
        Ok(())
    }

    async fn upload_final_output(
        &self,
        run_uid: &str,
        circuit_id: &str,
        output: &serde_json::Value,
    ) -> Result<()> {
        let body = serde_json::json!({
            "validator_key": self.wallet.hotkey_ss58(),
            "run_uid": run_uid,
            "circuit_id": circuit_id,
            "output": output,
        });

        let body_bytes = serde_json::to_vec(&body)?;
        let sig = self.sign_body(&body_bytes)?;

        let resp = self
            .http
            .post(format!("{}/proofs/{}/output", self.api_base_url, run_uid))
            .header("Content-Type", "application/json")
            .header("X-Request-Signature", sig)
            .body(body_bytes)
            .send()
            .await
            .context("uploading final output")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("upload_final_output returned {status}: {text}");
        }
        Ok(())
    }
}
