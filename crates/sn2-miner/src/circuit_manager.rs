use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::{info, warn};

pub struct CircuitManager {
    circuit_dir: PathBuf,
    storage_bucket: Option<String>,
    current_vk_hash: Mutex<Option<String>>,
}

impl CircuitManager {
    pub fn new(circuit_dir: &str, storage_bucket: Option<&str>) -> Self {
        Self {
            circuit_dir: PathBuf::from(circuit_dir),
            storage_bucket: storage_bucket.map(String::from),
            current_vk_hash: Mutex::new(None),
        }
    }

    pub fn calculate_vk_hash(&self) -> Result<Option<String>> {
        let compiled_model = self.circuit_dir.join("model.compiled");
        if !compiled_model.exists() {
            return Ok(None);
        }

        let data = std::fs::read(&compiled_model)
            .with_context(|| format!("reading {}", compiled_model.display()))?;

        let hash = Sha256::digest(&data);
        Ok(Some(hex::encode(hash)))
    }

    pub async fn get_commitment(&self) -> Result<Option<serde_json::Value>> {
        let vk_hash = match self.calculate_vk_hash()? {
            Some(h) => h,
            None => return Ok(None),
        };

        let settings_path = self.circuit_dir.join("settings.json");
        let has_settings = settings_path.exists();

        Ok(Some(serde_json::json!({
            "vk_hash": vk_hash,
            "has_settings": has_settings,
        })))
    }

    pub async fn upload_circuit_files(&self) -> Result<()> {
        let bucket = match &self.storage_bucket {
            Some(b) => b,
            None => {
                warn!("no storage bucket configured, skipping upload");
                return Ok(());
            }
        };

        let vk_hash = self
            .calculate_vk_hash()?
            .context("no compiled model to upload")?;

        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;
        let client = aws_sdk_s3::Client::new(&config);

        let files = ["model.compiled", "settings.json"];
        for file_name in &files {
            let file_path = self.circuit_dir.join(file_name);
            if !file_path.exists() {
                continue;
            }

            let body = aws_sdk_s3::primitives::ByteStream::from_path(&file_path)
                .await
                .with_context(|| format!("reading {file_name}"))?;

            let key = format!("{vk_hash}/{file_name}");

            client
                .put_object()
                .bucket(bucket)
                .key(&key)
                .body(body)
                .send()
                .await
                .with_context(|| format!("uploading {key}"))?;

            info!(key = %key, "uploaded circuit file");
        }

        Ok(())
    }

    pub fn start_monitor(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;

                let new_hash = match self.calculate_vk_hash() {
                    Ok(Some(h)) => h,
                    Ok(None) => continue,
                    Err(e) => {
                        warn!(error = %e, "calculating VK hash");
                        continue;
                    }
                };

                let needs_upload = {
                    let current = self.current_vk_hash.lock().await;
                    current.as_deref() != Some(&new_hash)
                };

                if needs_upload {
                    info!(vk_hash = %new_hash, "circuit files changed, uploading");

                    if let Err(e) = self.upload_circuit_files().await {
                        warn!(error = %e, "uploading circuit files");
                        continue;
                    }

                    *self.current_vk_hash.lock().await = Some(new_hash.clone());
                    info!(vk_hash = %new_hash, "circuit files uploaded");
                }
            }
        })
    }
}
