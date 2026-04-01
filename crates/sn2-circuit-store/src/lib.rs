use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use sn2_types::{
    Circuit, CircuitMetadata, CircuitPaths, CircuitType, ProofSystem, CIRCUIT_API_URL,
    CIRCUIT_CACHE_DIR, CIRCUIT_TIMEOUT_SECONDS, IGNORED_MODEL_HASHES,
};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

const SKIP_AUTO_DOWNLOAD: &[&str] = &["metadata.json", "full_model.onnx"];
const CIRCUIT_METADATA_FILENAME: &str = "circuit_metadata.json";
const DSLICE_READY_MARKER: &str = ".dslice_ready";
const REFRESH_INTERVAL_SECS: u64 = 600;

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn resolve_files(data: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let file_map = data.get("file_map").and_then(|v| v.as_object());

    if let Some(files_obj) = data.get("files").and_then(|v| v.as_object()) {
        if let Some(fmap) = file_map {
            let files_lower: HashMap<String, serde_json::Value> = files_obj
                .iter()
                .map(|(k, v)| (k.to_lowercase(), v.clone()))
                .collect();
            let mut resolved = serde_json::Map::new();
            for (filename, hash_val) in fmap {
                if let Some(hash) = hash_val.as_str() {
                    let hash_lower = hash.to_lowercase();
                    if let Some(url) = files_lower.get(&hash_lower) {
                        resolved.insert(filename.clone(), url.clone());
                    } else {
                        warn!(
                            filename,
                            hash, "file_map entry has no matching download URL in files"
                        );
                    }
                }
            }
            return resolved;
        }

        let all_hash_keys = !files_obj.is_empty() && files_obj.keys().all(|k| is_sha256_hex(k));
        if all_hash_keys {
            warn!(
                "API returned content-addressed files without file_map, cannot resolve filenames"
            );
            return serde_json::Map::new();
        }

        return files_obj.clone();
    }

    if let Some(arr) = data.get("files").and_then(|v| v.as_array()) {
        warn!(
            count = arr.len(),
            "API returned files as array without file_map, skipping downloads"
        );
    }

    serde_json::Map::new()
}

fn derive_checksums(
    file_map: Option<&serde_json::Map<String, serde_json::Value>>,
    explicit: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    if !explicit.is_empty() {
        return explicit.clone();
    }
    let Some(fmap) = file_map else {
        return serde_json::Map::new();
    };
    let mut checksums = serde_json::Map::new();
    for (filename, hash_val) in fmap {
        if let Some(hash) = hash_val.as_str() {
            if is_sha256_hex(hash) {
                checksums.insert(
                    filename.clone(),
                    serde_json::Value::String(hash.to_lowercase()),
                );
            }
        }
    }
    checksums
}

fn extract_content_hash_from_url(url: &str) -> Option<String> {
    let segment = url.rsplit('/').next()?;
    let clean = segment.split('?').next()?;
    if is_sha256_hex(clean) {
        Some(clean.to_lowercase())
    } else {
        None
    }
}

pub struct CircuitStore {
    circuits: HashMap<String, Circuit>,
    api_url: String,
    cache_dir: PathBuf,
    http: reqwest::Client,
    loopback: bool,
    api_url_overridden: bool,
    pinned_ids: HashSet<String>,
    inflight_downloads: Arc<Mutex<HashSet<String>>>,
    component_sha_map: HashMap<(String, String), String>,
}

impl CircuitStore {
    pub fn new(
        api_url_override: Option<&str>,
        loopback: bool,
        additional_circuits: Vec<String>,
    ) -> Self {
        let cache_dir = shellexpand::tilde(CIRCUIT_CACHE_DIR).to_string();
        let api_url_overridden = api_url_override.is_some();
        Self {
            circuits: HashMap::new(),
            api_url: api_url_override.unwrap_or(CIRCUIT_API_URL).to_string(),
            cache_dir: PathBuf::from(cache_dir),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .user_agent(concat!("sn2-circuit-store/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
            loopback,
            api_url_overridden,
            pinned_ids: additional_circuits
                .into_iter()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
            inflight_downloads: Arc::new(Mutex::new(HashSet::new())),
            component_sha_map: HashMap::new(),
        }
    }

    pub async fn load_circuits(&mut self) -> Result<()> {
        if self.loopback && !self.api_url_overridden {
            info!("loopback mode: loading all circuits from local cache");
            self.load_from_cache(&std::collections::HashSet::new());
            info!(count = self.circuits.len(), "circuits loaded");
            return Ok(());
        }

        let (mut api_circuits, complete) =
            self.fetch_circuits_from_api().await.unwrap_or_else(|e| {
                warn!(error = %e, "failed to fetch circuits from API, loading from cache only");
                (Vec::new(), false)
            });
        if !complete && !api_circuits.is_empty() {
            warn!("partial API response during startup, proceeding with available circuits");
        }

        let mut active_ids: HashSet<String> = api_circuits
            .iter()
            .filter_map(|c| c.get("id").and_then(|v| v.as_str()).map(String::from))
            .filter(|id| !IGNORED_MODEL_HASHES.contains(&id.as_str()))
            .collect();

        self.fetch_pinned_circuits(&mut active_ids, &mut api_circuits)
            .await;

        let mut load_ids = active_ids.clone();
        for id in &self.pinned_ids {
            load_ids.insert(id.clone());
        }
        self.load_from_cache(&load_ids);

        for circuit_data in &api_circuits {
            if let Some(id) = circuit_data.get("id").and_then(|v| v.as_str()) {
                if IGNORED_MODEL_HASHES.contains(&id) {
                    continue;
                }
                let is_loaded = self.circuits.contains_key(id);
                let is_dsperse = self.circuits.get(id).is_some_and(|c| {
                    c.metadata.circuit_type == CircuitType::DSPERSE_PROOF_GENERATION
                });
                if is_loaded && !is_dsperse {
                    continue;
                }
                match self.cache_and_load_circuit(id, circuit_data).await {
                    Ok((circuit, sha_mappings)) => {
                        if !is_loaded {
                            info!(id = id, name = %circuit.metadata.name, "loaded circuit from API");
                        }
                        self.component_sha_map.retain(|(mid, _), _| mid != id);
                        for (slice_name, comp_sha) in sha_mappings {
                            self.component_sha_map
                                .insert((id.to_string(), slice_name), comp_sha);
                        }
                        self.circuits.insert(id.to_string(), circuit);
                    }
                    Err(e) => {
                        warn!(id = id, error = ?e, "failed to cache circuit");
                    }
                }
            }
        }

        info!(count = self.circuits.len(), "circuits loaded");
        Ok(())
    }

    pub async fn ensure_circuit(&mut self, circuit_id: &str) -> Result<Circuit> {
        if IGNORED_MODEL_HASHES.contains(&circuit_id) {
            anyhow::bail!("circuit {} is in the ignored list", circuit_id);
        }

        if self.is_downloading(circuit_id) {
            anyhow::bail!("circuit {} has incomplete file downloads", circuit_id);
        }

        if let Some(circuit) = self.circuits.get(circuit_id) {
            return Ok(circuit.clone());
        }

        info!(id = circuit_id, "circuit not loaded, fetching from API");
        let data = self.fetch_circuit_or_model(circuit_id).await?;
        let (circuit, sha_mappings) = self.cache_and_load_circuit(circuit_id, &data).await?;
        self.component_sha_map
            .retain(|(mid, _), _| mid != circuit_id);
        for (slice_name, comp_sha) in sha_mappings {
            self.component_sha_map
                .insert((circuit_id.to_string(), slice_name), comp_sha);
        }
        self.circuits
            .insert(circuit_id.to_string(), circuit.clone());
        Ok(circuit)
    }

    pub async fn refresh_circuits(&mut self) -> Result<Vec<String>> {
        let (api_circuits, complete) = self.fetch_circuits_from_api().await?;
        let active_ids: HashSet<String> = api_circuits
            .iter()
            .filter_map(|c| c.get("id").and_then(|v| v.as_str()).map(String::from))
            .filter(|id| !IGNORED_MODEL_HASHES.contains(&id.as_str()))
            .collect();

        for circuit_data in &api_circuits {
            if let Some(id) = circuit_data.get("id").and_then(|v| v.as_str()) {
                if IGNORED_MODEL_HASHES.contains(&id) {
                    continue;
                }
                if self.circuits.contains_key(id) && !self.is_downloading(id) {
                    continue;
                }
                match self.cache_and_load_circuit(id, circuit_data).await {
                    Ok((circuit, sha_mappings)) => {
                        info!(id = id, name = %circuit.metadata.name, "loaded new circuit");
                        self.component_sha_map.retain(|(mid, _), _| mid != id);
                        for (slice_name, comp_sha) in sha_mappings {
                            self.component_sha_map
                                .insert((id.to_string(), slice_name), comp_sha);
                        }
                        self.circuits.insert(id.to_string(), circuit);
                    }
                    Err(e) => {
                        warn!(id = id, error = %e, "failed to load new circuit");
                    }
                }
            }
        }

        if active_ids.is_empty() || !complete {
            if !complete {
                warn!("partial API response, skipping circuit removal");
            } else {
                warn!("circuit API returned empty active set, skipping removal");
            }
            return Ok(Vec::new());
        }

        let removed: Vec<String> = self
            .circuits
            .keys()
            .filter(|id| {
                !active_ids.contains(id.as_str()) && !self.pinned_ids.contains(id.as_str())
            })
            .cloned()
            .collect();

        for id in &removed {
            info!(id = id, "removing deactivated circuit");
            self.circuits.remove(id);
        }

        Ok(removed)
    }

    pub fn get_benchmark_circuits(&self) -> Vec<Circuit> {
        self.circuits
            .values()
            .filter(|c| c.metadata.circuit_type != CircuitType::DSPERSE_PROOF_GENERATION)
            .cloned()
            .collect()
    }

    pub fn get_dsperse_circuits(&self) -> Vec<Circuit> {
        self.circuits
            .values()
            .filter(|c| c.metadata.circuit_type == CircuitType::DSPERSE_PROOF_GENERATION)
            .cloned()
            .collect()
    }

    pub fn get_circuit(&self, circuit_id: &str) -> Option<&Circuit> {
        self.circuits.get(circuit_id)
    }

    pub fn circuit_count(&self) -> usize {
        self.circuits.len()
    }

    pub fn is_downloading(&self, circuit_id: &str) -> bool {
        match self.inflight_downloads.lock() {
            Ok(set) => set.contains(circuit_id),
            Err(poisoned) => poisoned.into_inner().contains(circuit_id),
        }
    }

    pub fn is_dsperse_ready(&self, circuit_id: &str) -> bool {
        let cache_path = self.cache_dir.join(format!("model_{circuit_id}"));
        cache_path.join(DSLICE_READY_MARKER).exists()
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn component_sha(&self, model_id: &str, slice_name: &str) -> Option<&str> {
        self.component_sha_map
            .get(&(model_id.to_string(), slice_name.to_string()))
            .map(String::as_str)
    }

    pub const REFRESH_INTERVAL: u64 = REFRESH_INTERVAL_SECS;

    async fn fetch_circuit_or_model(&self, id: &str) -> Result<serde_json::Value> {
        let circuit_url = format!("{}/circuits/{}", self.api_url, id);
        let resp = self
            .http
            .get(&circuit_url)
            .send()
            .await
            .context("fetching circuit from API")?;

        if resp.status().is_success() {
            return resp.json().await.context("parsing circuit response");
        }

        if resp.status().as_u16() != 404 {
            anyhow::bail!("API returned {} for circuit {}", resp.status(), id);
        }

        info!(id, "circuit not found (404), trying models endpoint");
        let model_url = format!("{}/models/{}", self.api_url, id);
        let model_resp = self
            .http
            .get(&model_url)
            .send()
            .await
            .context("fetching model from API")?;

        if !model_resp.status().is_success() {
            anyhow::bail!(
                "API returned {} for circuit/model {}",
                model_resp.status(),
                id
            );
        }

        let model: serde_json::Value = model_resp.json().await.context("parsing model response")?;
        Ok(self.normalize_model_to_circuit(&model))
    }

    fn normalize_model_to_circuit(&self, model: &serde_json::Value) -> serde_json::Value {
        let metadata = model.get("metadata").cloned().unwrap_or_default();
        let composition = model.get("composition").cloned().unwrap_or_default();

        let str_field = |obj: &serde_json::Value, key: &str| -> String {
            obj.get(key)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };

        let proof_system = composition
            .get("components")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("proof_system"))
            .and_then(|v| v.as_str())
            .unwrap_or("JSTPROVE")
            .to_string();

        serde_json::json!({
            "id": str_field(model, "id"),
            "metadata": {
                "name": str_field(&metadata, "name"),
                "description": str_field(&metadata, "description"),
                "author": str_field(&metadata, "author"),
                "version": str_field(&metadata, "version"),
                "type": "DSPERSE_PROOF_GENERATION",
                "proof_system": proof_system,
                "netuid": metadata.get("netuid").cloned().unwrap_or(serde_json::Value::Null),
                "weights_version": metadata.get("weights_version").cloned().unwrap_or(serde_json::Value::Null),
                "timeout": metadata.get("timeout").cloned().unwrap_or(serde_json::json!(3600)),
                "input_schema": metadata.get("input_schema").cloned().unwrap_or_default(),
                "image_url": metadata.get("image_url").cloned().unwrap_or(serde_json::Value::Null),
            },
            "composition": composition,
            "files": {},
            "is_active": model.get("is_active").cloned().unwrap_or(serde_json::json!(1)),
            "created_at": model.get("created_at").cloned().unwrap_or_default(),
            "updated_at": model.get("updated_at").cloned().unwrap_or_default(),
        })
    }

    async fn fetch_circuits_from_api(&self) -> Result<(Vec<serde_json::Value>, bool)> {
        let mut all = Vec::new();
        let mut circuits_ok = false;
        let mut models_ok = false;

        let circuits_url = format!("{}/circuits", self.api_url);
        match self.http.get(&circuits_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<serde_json::Value>().await {
                    Ok(data) => {
                        if let Some(circuits) = data.get("circuits").and_then(|v| v.as_array()) {
                            all.extend(circuits.iter().cloned());
                            circuits_ok = true;
                        } else {
                            warn!("circuits response missing 'circuits' array");
                        }
                    }
                    Err(e) => warn!(error = %e, "failed to parse circuits response"),
                }
            }
            Ok(resp) => warn!(status = %resp.status(), "circuits endpoint returned error"),
            Err(e) => warn!(error = %e, "failed to reach circuits endpoint"),
        }

        let models_url = format!("{}/models", self.api_url);
        match self.http.get(&models_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<serde_json::Value>().await {
                    Ok(data) => {
                        if let Some(models) = data.get("models").and_then(|v| v.as_array()) {
                            let mut existing_ids: std::collections::HashSet<String> = all
                                .iter()
                                .filter_map(|c| {
                                    c.get("id").and_then(|v| v.as_str()).map(String::from)
                                })
                                .collect();
                            for model in models {
                                let Some(id) = model
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .filter(|s| !s.is_empty())
                                else {
                                    continue;
                                };
                                if existing_ids.insert(id.to_string()) {
                                    all.push(self.normalize_model_to_circuit(model));
                                }
                            }
                            models_ok = true;
                        } else {
                            warn!("models response missing 'models' array");
                        }
                    }
                    Err(e) => warn!(error = %e, "failed to parse models response"),
                }
            }
            Ok(resp) => warn!(status = %resp.status(), "models endpoint returned error"),
            Err(e) => warn!(error = %e, "failed to reach models endpoint"),
        }

        if !circuits_ok && !models_ok {
            anyhow::bail!("both circuits and models endpoints failed");
        }

        let complete = circuits_ok && models_ok;
        Ok((all, complete))
    }

    async fn fetch_pinned_circuits(
        &self,
        active_ids: &mut HashSet<String>,
        api_circuits: &mut Vec<serde_json::Value>,
    ) {
        for pinned_id in &self.pinned_ids {
            if active_ids.contains(pinned_id) {
                continue;
            }
            info!(id = %pinned_id, "fetching pinned circuit from API");
            let url = format!("{}/circuits/{}", self.api_url, pinned_id);
            match self.http.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<serde_json::Value>().await {
                        Ok(data) => {
                            active_ids.insert(pinned_id.clone());
                            api_circuits.push(data);
                        }
                        Err(e) => {
                            warn!(id = %pinned_id, error = %e, "failed to parse pinned circuit response");
                        }
                    }
                }
                Ok(resp) => {
                    warn!(id = %pinned_id, status = %resp.status(), "failed to fetch pinned circuit");
                }
                Err(e) => {
                    warn!(id = %pinned_id, error = %e, "failed to fetch pinned circuit");
                }
            }
        }
    }

    async fn cache_and_load_circuit(
        &self,
        circuit_id: &str,
        data: &serde_json::Value,
    ) -> Result<(Circuit, Vec<(String, String)>)> {
        let cache_path = self.cache_dir.join(format!("model_{circuit_id}"));
        let is_fresh = !cache_path.exists();
        std::fs::create_dir_all(&cache_path)
            .with_context(|| format!("creating cache dir {}", cache_path.display()))?;

        let metadata_value = data
            .get("metadata")
            .context("circuit data missing metadata")?;

        let metadata: CircuitMetadata =
            serde_json::from_value(metadata_value.clone()).context("parsing circuit metadata")?;

        let metadata_json = serde_json::to_string_pretty(metadata_value)?;
        let is_dsperse = metadata.circuit_type == CircuitType::DSPERSE_PROOF_GENERATION;

        let has_composition = data.get("composition").is_some_and(|c| {
            c.get("components")
                .and_then(|a| a.as_array())
                .is_some_and(|a| !a.is_empty())
        });

        let mut sha_mappings = Vec::new();
        if has_composition && is_dsperse {
            match self
                .download_composable_model(circuit_id, data, &cache_path)
                .await
            {
                Ok(mappings) => {
                    sha_mappings = mappings;
                }
                Err(e) => {
                    if is_fresh {
                        let _ = std::fs::remove_dir_all(&cache_path);
                    }
                    return Err(e.context("downloading composable model"));
                }
            }
        } else {
            let files = resolve_files(data);
            if files.is_empty() && data.get("files").is_some() {
                warn!(
                    circuit_id,
                    "API provided files but none could be resolved, circuit may be unusable"
                );
            }
            if !files.is_empty() {
                if is_dsperse {
                    let slices_dir = cache_path.join("slices");
                    std::fs::create_dir_all(&slices_dir)
                        .with_context(|| format!("creating slices dir {}", slices_dir.display()))?;
                }

                let file_map = data.get("file_map").and_then(|v| v.as_object());
                let explicit_checksums = data
                    .get("checksums")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default();
                let checksums = derive_checksums(file_map, &explicit_checksums);

                let deferred_downloads = match self
                    .download_circuit_files(&files, &cache_path, is_dsperse, &checksums)
                    .await
                {
                    Ok(d) => d,
                    Err(e) => {
                        if is_fresh {
                            let _ = std::fs::remove_dir_all(&cache_path);
                        }
                        return Err(e);
                    }
                };

                if !deferred_downloads.is_empty() {
                    self.spawn_deferred_downloads(circuit_id, &cache_path, deferred_downloads);
                } else if is_dsperse {
                    let _ = std::fs::write(cache_path.join(DSLICE_READY_MARKER), b"");
                }
            }
        }

        let metadata_path = cache_path.join(CIRCUIT_METADATA_FILENAME);
        std::fs::write(&metadata_path, metadata_json).context("writing metadata")?;

        let settings = load_settings(&cache_path);
        let proof_system = metadata
            .proof_system
            .parse::<ProofSystem>()
            .unwrap_or_else(|_| {
                warn!(raw = %metadata.proof_system, "unknown proof_system, defaulting to JSTPROVE");
                ProofSystem::JSTPROVE
            });

        Ok((
            Circuit {
                id: circuit_id.to_string(),
                paths: CircuitPaths::new(
                    &format!("model_{circuit_id}"),
                    &self.cache_dir.to_string_lossy(),
                ),
                metadata,
                proof_system,
                settings,
                timeout: CIRCUIT_TIMEOUT_SECONDS as f64,
            },
            sha_mappings,
        ))
    }

    async fn download_composable_model(
        &self,
        model_id: &str,
        data: &serde_json::Value,
        cache_path: &Path,
    ) -> Result<Vec<(String, String)>> {
        let composition = data.get("composition").context("missing composition")?;
        let components = composition
            .get("components")
            .and_then(|v| v.as_array())
            .context("missing composition.components")?;

        let slices_dir = cache_path.join("slices");
        std::fs::create_dir_all(&slices_dir)?;

        let total = components.len();
        info!(model_id, total, "downloading composable model components");

        self.inflight_downloads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(model_id.to_string());

        let result = self
            .download_composable_components(&slices_dir, components, model_id, data)
            .await;

        self.inflight_downloads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(model_id);

        match result {
            Ok(sha_mappings) => {
                let shas_persisted = serde_json::to_string(&sha_mappings)
                    .map_err(|e| {
                        warn!(model_id, error = %e, "failed to serialize component SHA mappings");
                    })
                    .and_then(|json| {
                        std::fs::write(cache_path.join("component_shas.json"), json).map_err(|e| {
                            warn!(model_id, error = %e, "failed to persist component SHA mappings");
                        })
                    })
                    .is_ok();
                if shas_persisted {
                    let _ = std::fs::write(cache_path.join(DSLICE_READY_MARKER), b"");
                }
                info!(model_id, total, "composable model download complete");
                Ok(sha_mappings)
            }
            Err(e) => {
                let _ = std::fs::remove_file(cache_path.join(DSLICE_READY_MARKER));
                Err(e)
            }
        }
    }

    fn sanitize_name<'a>(raw: &'a str, context: &str) -> Result<&'a str> {
        anyhow::ensure!(
            !raw.is_empty()
                && raw != ".."
                && !raw.contains('/')
                && !raw.contains('\\')
                && !raw.contains('\0'),
            "{context}: invalid name {raw:?}"
        );
        Ok(raw)
    }

    async fn download_composable_components(
        &self,
        slices_dir: &Path,
        components: &[serde_json::Value],
        model_id: &str,
        data: &serde_json::Value,
    ) -> Result<Vec<(String, String)>> {
        let total = components.len();
        let mut sha_mappings: Vec<(String, String)> = Vec::with_capacity(total);

        for (idx, comp) in components.iter().enumerate() {
            let default_name = format!("slice_{idx}");
            let raw_name = comp
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&default_name);
            let comp_name = Self::sanitize_name(raw_name, "component name")?;
            let comp_sha = comp
                .get("sha256")
                .and_then(|v| v.as_str())
                .context("component missing sha256")?;
            sha_mappings.push((comp_name.to_string(), comp_sha.to_string()));

            let comp_dir = slices_dir.join(comp_name);
            let stamp_path = comp_dir.join("component.sha");
            let sha_matches = stamp_path
                .exists()
                .then(|| std::fs::read_to_string(&stamp_path).ok())
                .flatten()
                .is_some_and(|s| s.trim() == comp_sha);
            if comp_dir.exists() && !sha_matches {
                info!(
                    comp_name,
                    comp_sha, "component SHA changed, clearing stale cache"
                );
                std::fs::remove_dir_all(&comp_dir)
                    .with_context(|| format!("removing stale component cache for {comp_name}"))?;
            }

            let comp_files = comp
                .get("files")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let weights = comp
                .get("weights")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            let has_circuit = comp_files.iter().any(|f| f.as_str() == Some("circuit.bin"));

            if has_circuit {
                let bundle_dir = slices_dir
                    .join(comp_name)
                    .join("jstprove")
                    .join("circuit.bundle");
                std::fs::create_dir_all(&bundle_dir)?;

                for file_val in &comp_files {
                    let raw_filename = file_val.as_str().unwrap_or_default();
                    if raw_filename.is_empty() {
                        continue;
                    }
                    let filename = Self::sanitize_name(raw_filename, "component file")?;
                    let dest = bundle_dir.join(filename);
                    if dest.exists() {
                        continue;
                    }
                    let url = format!(
                        "{}/components/{}/files/{}",
                        self.api_url, comp_sha, filename
                    );
                    self.download_file(&url, &dest).await.with_context(|| {
                        format!("downloading component file {comp_name}/{filename}")
                    })?;
                }
            }

            let payload_dir = slices_dir.join(comp_name).join("payload");
            std::fs::create_dir_all(&payload_dir)?;

            for wb in &weights {
                let wb_sha = wb
                    .get("sha256")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .with_context(|| format!("{comp_name}: weight blob missing sha256"))?;
                let default_wb = format!("{comp_name}.onnx");
                let raw_wb = wb
                    .get("filename")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&default_wb);
                let wb_filename = Self::sanitize_name(raw_wb, "weight blob file")?;
                let dest = payload_dir.join(wb_filename);
                if dest.exists() {
                    continue;
                }
                let url = format!("{}/models/wb/{}", self.api_url, wb_sha);
                self.download_file(&url, &dest)
                    .await
                    .with_context(|| format!("downloading weight blob for {comp_name}"))?;
            }

            if let Err(e) = std::fs::write(&stamp_path, comp_sha) {
                warn!(comp_name, error = %e, "failed to write component SHA stamp");
            }

            if (idx + 1) % 50 == 0 || idx + 1 == total {
                info!(
                    model_id,
                    progress = idx + 1,
                    total,
                    "composable model download progress"
                );
            }
        }

        if let Some(artifacts) = data
            .get("composition")
            .and_then(|c| c.get("artifacts"))
            .and_then(|a| a.as_array())
        {
            for artifact in artifacts {
                let sha = artifact
                    .get("sha256")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .context("model artifact missing sha256")?;
                let raw_filename = artifact
                    .get("filename")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let filename = Self::sanitize_name(raw_filename, "model artifact")?;
                let dest = slices_dir.join(filename);
                if dest.exists() {
                    continue;
                }
                let url = format!("{}/models/wb/{}", self.api_url, sha);
                self.download_file(&url, &dest)
                    .await
                    .with_context(|| format!("downloading model artifact {filename}"))?;
                info!(filename, "downloaded model artifact");
            }
        }

        Ok(sha_mappings)
    }

    async fn download_circuit_files(
        &self,
        files: &serde_json::Map<String, serde_json::Value>,
        cache_path: &Path,
        is_dsperse: bool,
        checksums: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Vec<(String, PathBuf)>> {
        let mut deferred_downloads: Vec<(String, PathBuf)> = Vec::new();

        for (filename, url_val) in files {
            let safe_name = Path::new(filename).file_name().and_then(|n| n.to_str());
            if safe_name != Some(filename.as_str()) {
                anyhow::bail!("rejecting filename with path traversal: {filename}");
            }

            let skip = if is_dsperse {
                filename == "full_model.onnx"
            } else {
                SKIP_AUTO_DOWNLOAD.contains(&filename.as_str())
            };
            if skip {
                continue;
            }

            if is_dsperse && filename.ends_with(".dslice") {
                let cache_path = cache_path.to_path_buf();
                let filename_owned = filename.clone();
                let checksums = checksums.clone();
                let url_val = url_val.clone();
                match tokio::task::spawn_blocking(move || {
                    resolve_dslice_download(&cache_path, &filename_owned, &checksums, &url_val)
                })
                .await
                {
                    Ok(Ok(Some(url))) => deferred_downloads.push(url),
                    Ok(Ok(None)) => {}
                    Ok(Err(e)) => {
                        return Err(e.context(format!("dslice preflight failed: {filename}")));
                    }
                    Err(e) => {
                        anyhow::bail!("spawn_blocking panicked for dslice check: {filename}: {e}");
                    }
                }
                continue;
            }

            let dest =
                if is_dsperse && (filename == "metadata.json" || filename == "metadata.msgpack") {
                    cache_path.join("slices").join(filename)
                } else {
                    cache_path.join(filename)
                };
            if dest.exists() {
                continue;
            }
            let url = url_val.as_str().with_context(|| {
                format!("circuit file {filename} has non-string URL value: {url_val}")
            })?;
            self.download_file(url, &dest)
                .await
                .with_context(|| format!("downloading required circuit file: {filename}"))?;
        }

        Ok(deferred_downloads)
    }

    fn spawn_deferred_downloads(
        &self,
        circuit_id: &str,
        cache_path: &Path,
        deferred_downloads: Vec<(String, PathBuf)>,
    ) {
        if !self
            .inflight_downloads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(circuit_id.to_string())
        {
            return;
        }
        let _ = std::fs::remove_file(cache_path.join(DSLICE_READY_MARKER));
        let count = deferred_downloads.len();
        let http = self.http.clone();
        let inflight = Arc::clone(&self.inflight_downloads);
        let cid = circuit_id.to_string();
        let marker_path = cache_path.join(DSLICE_READY_MARKER);
        info!(circuit = %circuit_id, files = count, "spawning background dslice downloads");
        tokio::spawn(async move {
            let mut downloaded = 0usize;
            let mut failed = 0usize;
            for (url, dest) in &deferred_downloads {
                if dest.exists() {
                    downloaded += 1;
                    continue;
                }
                match download_file_static(&http, url, dest).await {
                    Ok(()) => {
                        downloaded += 1;
                        if downloaded % 20 == 0 || downloaded == count {
                            info!(progress = %format!("{downloaded}/{count}"), "dslice download progress");
                        }
                    }
                    Err(e) => {
                        failed += 1;
                        warn!(file = %dest.display(), error = %e, "failed to download dslice file");
                    }
                }
            }
            if failed == 0 {
                let _ = std::fs::write(&marker_path, b"");
            } else {
                warn!(circuit = %cid, failed, "dslice downloads incomplete, will retry on next refresh");
            }
            match inflight.lock() {
                Ok(mut set) => {
                    set.remove(&cid);
                }
                Err(poisoned) => {
                    poisoned.into_inner().remove(&cid);
                }
            }
            info!(
                count = downloaded,
                failed, "dslice background downloads complete"
            );
        });
    }

    fn load_from_cache(&mut self, active_ids: &HashSet<String>) {
        let cache_dir = &self.cache_dir;
        let entries = match std::fs::read_dir(cache_dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            if let Some((circuit_id, circuit)) =
                self.try_load_cache_entry(&entry, active_ids, cache_dir)
            {
                let shas_path = entry.path().join("component_shas.json");
                match std::fs::read_to_string(&shas_path) {
                    Ok(data) => match serde_json::from_str::<Vec<(String, String)>>(&data) {
                        Ok(mappings) => {
                            for (slice_name, comp_sha) in mappings {
                                self.component_sha_map
                                    .insert((circuit_id.clone(), slice_name), comp_sha);
                            }
                        }
                        Err(e) => {
                            warn!(id = %circuit_id, path = %shas_path.display(), error = %e, "corrupt component_shas.json");
                        }
                    },
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        warn!(id = %circuit_id, path = %shas_path.display(), error = %e, "failed to read component_shas.json");
                    }
                    _ => {}
                }
                self.circuits.insert(circuit_id, circuit);
            }
        }
    }

    fn try_load_cache_entry(
        &self,
        entry: &std::fs::DirEntry,
        active_ids: &HashSet<String>,
        cache_dir: &Path,
    ) -> Option<(String, Circuit)> {
        let dir_name = entry.file_name().to_string_lossy().to_string();
        let circuit_id = match dir_name.strip_prefix("model_") {
            Some(id) if id.len() == 64 => id.to_string(),
            _ => return None,
        };

        if IGNORED_MODEL_HASHES.contains(&circuit_id.as_str()) {
            return None;
        }
        if !active_ids.is_empty() && !active_ids.contains(&circuit_id) {
            return None;
        }
        if self.circuits.contains_key(&circuit_id) {
            return None;
        }

        let metadata_path = entry.path().join(CIRCUIT_METADATA_FILENAME);
        if !metadata_path.exists() {
            return None;
        }

        match load_circuit_from_cache(&circuit_id, &entry.path(), cache_dir) {
            Ok(circuit) => {
                if circuit.metadata.circuit_type == CircuitType::DSPERSE_PROOF_GENERATION {
                    migrate_dslice_layout(&entry.path());
                    if !entry.path().join(DSLICE_READY_MARKER).exists() {
                        warn!(id = %circuit_id, "skipping incomplete DSPERSE circuit from cache");
                        return None;
                    }
                }
                Some((circuit_id, circuit))
            }
            Err(e) => {
                warn!(id = circuit_id, error = %e, "failed to load cached circuit");
                None
            }
        }
    }

    async fn download_file(&self, url: &str, dest: &Path) -> Result<()> {
        download_file_static(&self.http, url, dest).await
    }
}

async fn download_file_static(http: &reqwest::Client, url: &str, dest: &Path) -> Result<()> {
    let resp = http
        .get(url)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await
        .context("downloading file")?;

    if !resp.status().is_success() {
        anyhow::bail!("download returned {}", resp.status());
    }

    let expected_sha256 = resp
        .headers()
        .get("x-checksum-sha256")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_lowercase())
        .or_else(|| extract_content_hash_from_url(url));

    let partial = dest.with_extension("partial");

    let result = async {
        let max_bytes = (sn2_types::MAX_CIRCUIT_SIZE_GB as u64) * 1024 * 1024 * 1024;
        if let Some(content_len) = resp.content_length() {
            anyhow::ensure!(
                content_len <= max_bytes,
                "download too large: {content_len} bytes (max: {max_bytes} bytes)"
            );
        }

        let file = tokio::fs::File::create(&partial)
            .await
            .with_context(|| format!("creating {}", partial.display()))?;
        let mut writer = tokio::io::BufWriter::new(file);
        let mut stream = resp.bytes_stream();
        let mut downloaded: u64 = 0;
        let mut hasher = Sha256::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading download stream")?;
            downloaded += chunk.len() as u64;
            anyhow::ensure!(
                downloaded <= max_bytes,
                "download exceeded max size: {downloaded} > {max_bytes} bytes"
            );
            hasher.update(&chunk);
            writer
                .write_all(&chunk)
                .await
                .with_context(|| format!("writing to {}", partial.display()))?;
        }

        writer
            .flush()
            .await
            .with_context(|| format!("flushing {}", partial.display()))?;

        if let Some(expected) = &expected_sha256 {
            let actual = hex::encode(hasher.finalize());
            anyhow::ensure!(
                &actual == expected,
                "SHA-256 mismatch: expected {expected}, got {actual}"
            );
        }

        tokio::fs::rename(&partial, dest)
            .await
            .with_context(|| format!("renaming {} to {}", partial.display(), dest.display()))
    }
    .await;

    if result.is_err() {
        let _ = tokio::fs::remove_file(&partial).await;
    }

    result
}

fn load_circuit_from_cache(circuit_id: &str, dir: &Path, cache_dir: &Path) -> Result<Circuit> {
    let metadata_path = dir.join(CIRCUIT_METADATA_FILENAME);
    let metadata_str = std::fs::read_to_string(&metadata_path).context("reading metadata")?;
    let metadata: CircuitMetadata =
        serde_json::from_str(&metadata_str).context("parsing cached metadata")?;
    let settings = load_settings(dir);
    let proof_system = metadata
        .proof_system
        .parse::<ProofSystem>()
        .unwrap_or(ProofSystem::JSTPROVE);

    Ok(Circuit {
        id: circuit_id.to_string(),
        paths: CircuitPaths::new(&format!("model_{circuit_id}"), &cache_dir.to_string_lossy()),
        metadata,
        proof_system,
        settings,
        timeout: CIRCUIT_TIMEOUT_SECONDS as f64,
    })
}

fn load_settings(dir: &Path) -> HashMap<String, serde_json::Value> {
    let settings_path = dir.join("settings.json");
    std::fs::read_to_string(&settings_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn migrate_dslice_layout(model_dir: &Path) {
    let slices_dir = model_dir.join("slices");
    if slices_dir.exists() {
        return;
    }
    let entries = match std::fs::read_dir(model_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let dslice_files: Vec<_> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".dslice"))
        .collect();
    if dslice_files.is_empty() {
        return;
    }
    if std::fs::create_dir_all(&slices_dir).is_err() {
        return;
    }
    for entry in dslice_files {
        let dest = slices_dir.join(entry.file_name());
        if let Err(e) = std::fs::rename(entry.path(), &dest) {
            warn!(
                file = %entry.file_name().to_string_lossy(),
                error = %e,
                "failed to migrate dslice file to slices/"
            );
        }
    }
    info!(dir = %model_dir.display(), "migrated dslice files to slices/ subdirectory");
}

fn validate_slice_id(slice_id: &str) -> Result<()> {
    anyhow::ensure!(
        !slice_id.contains('/') && !slice_id.contains('\\') && !slice_id.contains(".."),
        "invalid slice_id: {slice_id}"
    );
    Ok(())
}

pub fn ensure_slice_extracted(slices_dir: &Path, slice_id: &str) -> Result<()> {
    validate_slice_id(slice_id)?;
    let extract_dir = slices_dir.join(slice_id);
    if extract_dir.exists() {
        return Ok(());
    }
    let archive = slices_dir.join(format!("{slice_id}.dslice"));
    if !archive.exists() {
        anyhow::bail!("dslice archive not found: {}", archive.display());
    }
    let tmp_dir = slices_dir.join(format!(".{slice_id}.extracting.{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).with_context(|| format!("creating {}", tmp_dir.display()))?;
    let file =
        std::fs::File::open(&archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut zip =
        zip::ZipArchive::new(file).with_context(|| format!("reading zip {}", archive.display()))?;
    if let Err(e) = zip
        .extract(&tmp_dir)
        .with_context(|| format!("extracting {} to {}", archive.display(), tmp_dir.display()))
    {
        std::fs::remove_dir_all(&tmp_dir).ok();
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp_dir, &extract_dir) {
        std::fs::remove_dir_all(&tmp_dir).ok();
        if extract_dir.exists() {
            return Ok(());
        }
        return Err(anyhow::anyhow!(
            "renaming {} to {}: {e}",
            tmp_dir.display(),
            extract_dir.display()
        ));
    }
    Ok(())
}

fn resolve_dslice_download(
    cache_path: &Path,
    filename: &str,
    checksums: &serde_json::Map<String, serde_json::Value>,
    url_val: &serde_json::Value,
) -> Result<Option<(String, PathBuf)>> {
    let archive_dest = cache_path.join("slices").join(filename);
    let slice_name = filename.trim_end_matches(".dslice");
    let extracted_dir = cache_path.join("slices").join(slice_name);
    if archive_dest.exists() {
        if file_checksum_valid(&archive_dest, checksums, filename) {
            return Ok(None);
        }
        warn!(file = %filename, "SHA-256 mismatch, removing corrupted dslice");
        std::fs::remove_file(&archive_dest)
            .with_context(|| format!("removing corrupted archive {}", archive_dest.display()))?;
        if extracted_dir.exists() {
            std::fs::remove_dir_all(&extracted_dir).with_context(|| {
                format!("removing stale extracted dir {}", extracted_dir.display())
            })?;
        }
    } else if extracted_dir.exists() {
        return Ok(None);
    }
    let url = url_val
        .as_str()
        .with_context(|| format!("dslice {filename} has non-string URL value: {url_val}"))?;
    Ok(Some((url.to_string(), archive_dest)))
}

fn file_checksum_valid(
    path: &Path,
    checksums: &serde_json::Map<String, serde_json::Value>,
    filename: &str,
) -> bool {
    let Some(expected) = checksums.get(filename).and_then(|v| v.as_str()) else {
        return true;
    };
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let Ok(n) = std::io::Read::read(&mut reader, &mut buf) else {
            return false;
        };
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    hex::encode(hasher.finalize()) == expected
}

pub fn cleanup_extracted_slice(slices_dir: &Path, slice_id: &str) {
    if let Err(e) = validate_slice_id(slice_id) {
        tracing::warn!(slice_id, error = %e, "refusing to clean up slice with invalid id");
        return;
    }
    let extract_dir = slices_dir.join(slice_id);
    if extract_dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&extract_dir) {
            tracing::warn!(dir = %extract_dir.display(), error = %e, "failed to remove extracted slice dir");
        }
    }
}
