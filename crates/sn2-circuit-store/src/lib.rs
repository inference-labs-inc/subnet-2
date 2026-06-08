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

const CIRCUIT_METADATA_FILENAME: &str = "circuit_metadata.json";
const DSLICE_READY_MARKER: &str = ".dslice_ready";
const REFRESH_INTERVAL_SECS: u64 = 600;

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

struct WeightRef {
    sha: String,
    filename: String,
}

struct ParsedComponent {
    name: String,
    sha: String,
    files: Vec<String>,
    weights: Vec<WeightRef>,
    has_circuit: bool,
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
        cache_dir_override: Option<&str>,
    ) -> Self {
        let cache_dir =
            shellexpand::tilde(cache_dir_override.unwrap_or(CIRCUIT_CACHE_DIR)).to_string();
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

        if complete {
            self.purge_stale_cache_dirs(&load_ids);
        }

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
            self.component_sha_map.retain(|(mid, _), _| mid != id);
        }

        Ok(removed)
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

        let has_composition = data.get("composition").is_some_and(|c| {
            c.get("components")
                .and_then(|a| a.as_array())
                .is_some_and(|a| !a.is_empty())
        });

        let sha_mappings = if has_composition {
            match self
                .download_composable_model(circuit_id, data, &cache_path)
                .await
            {
                Ok(mappings) => mappings,
                Err(e) => {
                    if is_fresh {
                        let _ = std::fs::remove_dir_all(&cache_path);
                    }
                    return Err(e.context("downloading composable model"));
                }
            }
        } else {
            warn!(circuit_id, "non-composable circuit skipped (deprecated)");
            Vec::new()
        };

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
        let parsed = Self::parse_components(components)?;

        let stale = Self::find_stale_components(slices_dir, &parsed);
        Self::ensure_component_dirs(slices_dir, &parsed)?;

        for (idx, comp) in parsed.iter().enumerate() {
            let needs_download = stale.contains(&comp.name);
            self.download_component_files(slices_dir, comp, needs_download)
                .await?;
            Self::write_component_stamp(slices_dir, comp)?;
            if (idx + 1) % 50 == 0 || idx + 1 == total {
                info!(
                    model_id,
                    progress = idx + 1,
                    total,
                    "composable model download progress"
                );
            }
        }

        self.download_model_artifacts(slices_dir, data).await?;

        Ok(parsed
            .iter()
            .map(|c| (c.name.clone(), c.sha.clone()))
            .collect())
    }

    fn parse_components(components: &[serde_json::Value]) -> Result<Vec<ParsedComponent>> {
        components
            .iter()
            .enumerate()
            .map(|(idx, comp)| {
                let default_name = format!("slice_{idx}");
                let raw_name = comp
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&default_name);
                let name = Self::sanitize_name(raw_name, "component name")?.to_string();
                let sha = comp
                    .get("sha256")
                    .and_then(|v| v.as_str())
                    .context("component missing sha256")?
                    .to_string();
                let files: Vec<String> = comp
                    .get("files")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let weights: Vec<WeightRef> = comp
                    .get("weights")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .enumerate()
                            .filter_map(|(i, w)| {
                                let wb_sha = w.get("sha256")?.as_str()?.to_string();
                                let fallback = format!("{name}_weight_{i}.onnx");
                                let filename = w
                                    .get("filename")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(&fallback)
                                    .to_string();
                                Some(WeightRef {
                                    sha: wb_sha,
                                    filename,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let has_circuit = files.iter().any(|f| f == "circuit.bin");
                Ok(ParsedComponent {
                    name,
                    sha,
                    files,
                    weights,
                    has_circuit,
                })
            })
            .collect()
    }

    fn find_stale_components(slices_dir: &Path, components: &[ParsedComponent]) -> HashSet<String> {
        let mut stale = HashSet::new();
        for comp in components {
            let comp_dir = slices_dir.join(&comp.name);
            let stamp_path = comp_dir.join("component.sha");
            let needs_redownload = match std::fs::read_to_string(&stamp_path) {
                Ok(stamp) if stamp.trim() == comp.sha => {
                    if comp.has_circuit {
                        let bundle_dir = comp_dir.join("jstprove").join("circuit.bundle");
                        let has_circuit_bin = bundle_dir.join("circuit.bin").exists();
                        if !has_circuit_bin {
                            info!(
                                name = comp.name,
                                "component missing circuit.bin, will re-download"
                            );
                        }
                        !has_circuit_bin
                    } else {
                        false
                    }
                }
                Ok(stamp) => {
                    info!(
                        name = comp.name,
                        sha = comp.sha,
                        old_sha = stamp.trim(),
                        "component SHA changed, will re-download"
                    );
                    true
                }
                Err(_) => {
                    if !comp_dir.exists() {
                        true
                    } else if comp.has_circuit {
                        let has_circuit_bin = comp_dir
                            .join("jstprove")
                            .join("circuit.bundle")
                            .join("circuit.bin")
                            .exists();
                        !has_circuit_bin
                    } else {
                        let payload_dir = comp_dir.join("payload");
                        let has_payload = match payload_dir.read_dir() {
                            Ok(mut d) => d.next().is_some(),
                            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                                warn!(
                                    name = comp.name,
                                    path = %payload_dir.display(),
                                    error = %e,
                                    "permission denied reading payload dir, skipping re-download"
                                );
                                true
                            }
                            Err(_) => false,
                        };
                        !has_payload
                    }
                }
            };
            if needs_redownload {
                stale.insert(comp.name.clone());
            }
        }
        if !stale.is_empty() {
            info!(count = stale.len(), "components requiring download");
        }
        stale
    }

    fn ensure_component_dirs(slices_dir: &Path, components: &[ParsedComponent]) -> Result<()> {
        for comp in components {
            let comp_dir = slices_dir.join(&comp.name);
            if comp.has_circuit {
                std::fs::create_dir_all(comp_dir.join("jstprove").join("circuit.bundle"))?;
            }
            std::fs::create_dir_all(comp_dir.join("payload"))?;
        }
        Ok(())
    }

    async fn download_component_files(
        &self,
        slices_dir: &Path,
        comp: &ParsedComponent,
        force: bool,
    ) -> Result<()> {
        if comp.has_circuit {
            let bundle_dir = slices_dir
                .join(&comp.name)
                .join("jstprove")
                .join("circuit.bundle");
            for raw_filename in &comp.files {
                if raw_filename.is_empty() {
                    continue;
                }
                let filename = Self::sanitize_name(raw_filename, "component file")?;
                let dest = bundle_dir.join(filename);
                if dest.exists() && !force {
                    continue;
                }
                if force && dest.exists() {
                    let _ = std::fs::remove_file(&dest);
                }
                if Self::try_hardlink_from_component_cache(
                    &self.cache_dir,
                    slices_dir,
                    &comp.sha,
                    &PathBuf::from("jstprove/circuit.bundle").join(filename),
                    &dest,
                ) {
                    continue;
                }
                let url = format!(
                    "{}/components/{}/files/{}",
                    self.api_url, comp.sha, filename
                );
                self.download_file(&url, &dest)
                    .await
                    .with_context(|| format!("downloading {}/{}", comp.name, filename))?;
            }
        }

        let payload_dir = slices_dir.join(&comp.name).join("payload");
        for wb in &comp.weights {
            let filename = Self::sanitize_name(&wb.filename, "weight blob file")?;
            let dest = payload_dir.join(filename);
            if dest.exists() && !force {
                continue;
            }
            if force && dest.exists() {
                let _ = std::fs::remove_file(&dest);
                let _ = std::fs::remove_file(Self::sha_stamp_path(&dest));
            }
            if Self::try_hardlink_from_weight_cache(&self.cache_dir, slices_dir, &wb.sha, &dest) {
                Self::write_weight_sha_stamp(&dest, &wb.sha);
                continue;
            }
            let url = format!("{}/models/wb/{}", self.api_url, wb.sha);
            self.download_file(&url, &dest)
                .await
                .with_context(|| format!("downloading weight blob for {}", comp.name))?;
            Self::write_weight_sha_stamp(&dest, &wb.sha);
        }
        Ok(())
    }

    fn sha_stamp_path(dest: &Path) -> PathBuf {
        let mut p = dest.as_os_str().to_os_string();
        p.push(".sha");
        PathBuf::from(p)
    }

    fn write_weight_sha_stamp(dest: &Path, sha: &str) {
        let _ = std::fs::write(Self::sha_stamp_path(dest), sha.as_bytes());
    }

    /// True when `path` is a `model_<sha256>` directory under the cache root.
    fn is_model_cache_dir(path: &Path) -> bool {
        if !path.is_dir() {
            return false;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => return false,
        };
        match name.strip_prefix("model_") {
            Some(id) => is_sha256_hex(id),
            None => false,
        }
    }

    /// Scan sibling cached circuits for a component stamped with the same SHA
    /// and hard-link the requested file from there. Returns true on success.
    fn try_hardlink_from_component_cache(
        cache_dir: &Path,
        current_slices_dir: &Path,
        component_sha: &str,
        relative_path: &Path,
        dest: &Path,
    ) -> bool {
        let root_entries = match std::fs::read_dir(cache_dir) {
            Ok(entries) => entries,
            Err(_) => return false,
        };
        for entry in root_entries.flatten() {
            let path = entry.path();
            if !Self::is_model_cache_dir(&path) {
                continue;
            }
            let candidate_slices = path.join("slices");
            if candidate_slices == current_slices_dir {
                continue;
            }
            let slice_entries = match std::fs::read_dir(&candidate_slices) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for slice_entry in slice_entries.flatten() {
                let slice_path = slice_entry.path();
                let stamp = slice_path.join("component.sha");
                let matches = match std::fs::read_to_string(&stamp) {
                    Ok(stamp_value) => stamp_value.trim() == component_sha,
                    Err(_) => false,
                };
                if !matches {
                    continue;
                }
                let source = slice_path.join(relative_path);
                if !source.exists() {
                    continue;
                }
                if let Some(parent) = dest.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if std::fs::hard_link(&source, dest).is_ok() {
                    info!(
                        source = %source.display(),
                        dest = %dest.display(),
                        sha = component_sha,
                        "hard-linked component file from sibling circuit cache",
                    );
                    return true;
                }
            }
        }
        false
    }

    /// Scan sibling cached circuits for a weight blob whose `.sha` stamp
    /// matches `weight_sha` and hard-link it into place. Returns true on
    /// success. Candidates without a stamp are skipped rather than hashed so
    /// large payloads do not trigger multi-gigabyte re-reads under contention.
    fn try_hardlink_from_weight_cache(
        cache_dir: &Path,
        current_slices_dir: &Path,
        weight_sha: &str,
        dest: &Path,
    ) -> bool {
        let root_entries = match std::fs::read_dir(cache_dir) {
            Ok(entries) => entries,
            Err(_) => return false,
        };
        for entry in root_entries.flatten() {
            let path = entry.path();
            if !Self::is_model_cache_dir(&path) {
                continue;
            }
            let candidate_slices = path.join("slices");
            if candidate_slices == current_slices_dir {
                continue;
            }
            let slice_entries = match std::fs::read_dir(&candidate_slices) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for slice_entry in slice_entries.flatten() {
                let payload_dir = slice_entry.path().join("payload");
                let payload_files = match std::fs::read_dir(&payload_dir) {
                    Ok(entries) => entries,
                    Err(_) => continue,
                };
                for payload_entry in payload_files.flatten() {
                    let candidate = payload_entry.path();
                    if !candidate.is_file() {
                        continue;
                    }
                    if candidate
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|ext| ext == "sha")
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    let stamp_path = Self::sha_stamp_path(&candidate);
                    let recorded = match std::fs::read_to_string(&stamp_path) {
                        Ok(value) => value.trim().to_string(),
                        Err(_) => continue,
                    };
                    if recorded != weight_sha {
                        continue;
                    }
                    if let Some(parent) = dest.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if std::fs::hard_link(&candidate, dest).is_ok() {
                        info!(
                            source = %candidate.display(),
                            dest = %dest.display(),
                            sha = weight_sha,
                            "hard-linked weight blob from sibling circuit cache",
                        );
                        return true;
                    }
                }
            }
        }
        false
    }

    fn write_component_stamp(slices_dir: &Path, comp: &ParsedComponent) -> Result<()> {
        let stamp_path = slices_dir.join(&comp.name).join("component.sha");
        std::fs::write(&stamp_path, &comp.sha)
            .with_context(|| format!("writing component SHA stamp for {}", comp.name))
    }

    async fn download_model_artifacts(
        &self,
        slices_dir: &Path,
        data: &serde_json::Value,
    ) -> Result<()> {
        let artifacts = match data
            .get("composition")
            .and_then(|c| c.get("artifacts"))
            .and_then(|a| a.as_array())
        {
            Some(a) => a,
            None => return Ok(()),
        };
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
        Ok(())
    }

    fn purge_stale_cache_dirs(&self, retain_ids: &HashSet<String>) {
        let entries = match std::fs::read_dir(&self.cache_dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        let downloading = self
            .inflight_downloads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();

        let mut purged = 0usize;
        for entry in entries.flatten() {
            let dir_name = entry.file_name().to_string_lossy().to_string();
            let circuit_id = match dir_name.strip_prefix("model_") {
                Some(id) if is_sha256_hex(id) => id,
                _ => continue,
            };

            if retain_ids.contains(circuit_id) || downloading.contains(circuit_id) {
                continue;
            }

            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    purged += 1;
                }
                Err(e) => {
                    warn!(id = circuit_id, error = %e, "failed to purge stale cache directory");
                }
            }
        }

        if purged > 0 {
            info!(purged, "purged stale model cache directories");
        }
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
                if circuit.metadata.circuit_type == CircuitType::DSPERSE_PROOF_GENERATION
                    && !entry.path().join(DSLICE_READY_MARKER).exists()
                {
                    warn!(id = %circuit_id, "skipping incomplete DSPERSE circuit from cache");
                    return None;
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
        .or_else(|| {
            let segment = url.rsplit('/').next()?;
            let clean = segment.split('?').next()?;
            if is_sha256_hex(clean) {
                Some(clean.to_lowercase())
            } else {
                None
            }
        });

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
