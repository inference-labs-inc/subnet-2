use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sn2_types::{
    Circuit, CircuitMetadata, CircuitPaths, CircuitType, ProofSystem, CIRCUIT_API_URL,
    CIRCUIT_CACHE_DIR, CIRCUIT_TIMEOUT_SECONDS, IGNORED_MODEL_HASHES,
};
use tracing::{info, warn};

const SKIP_AUTO_DOWNLOAD: &[&str] = &["metadata.json", "full_model.onnx"];
const CIRCUIT_METADATA_FILENAME: &str = "circuit_metadata.json";
const REFRESH_INTERVAL_SECS: u64 = 600;

pub struct CircuitStore {
    circuits: HashMap<String, Circuit>,
    api_url: String,
    cache_dir: PathBuf,
    http: reqwest::Client,
    loopback: bool,
    api_url_overridden: bool,
    pinned_ids: std::collections::HashSet<String>,
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
                .build()
                .unwrap_or_default(),
            loopback,
            api_url_overridden,
            pinned_ids: additional_circuits
                .into_iter()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }

    pub async fn load_circuits(&mut self) -> Result<()> {
        if self.loopback && !self.api_url_overridden {
            info!("loopback mode: loading all circuits from local cache");
            self.load_from_cache(&std::collections::HashSet::new());
            info!(count = self.circuits.len(), "circuits loaded");
            return Ok(());
        }

        let mut api_circuits = self.fetch_circuits_from_api().await.unwrap_or_else(|e| {
            warn!(error = %e, "failed to fetch circuits from API, loading from cache only");
            Vec::new()
        });

        let mut active_ids: std::collections::HashSet<String> = api_circuits
            .iter()
            .filter_map(|c| c.get("id").and_then(|v| v.as_str()).map(String::from))
            .collect();

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

        if active_ids.is_empty() {
            self.load_from_cache(&active_ids);
        } else {
            let mut load_ids = active_ids.clone();
            for id in &self.pinned_ids {
                load_ids.insert(id.clone());
            }
            self.load_from_cache(&load_ids);
        }

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
                    Ok(circuit) => {
                        if !is_loaded {
                            info!(id = id, name = %circuit.metadata.name, "loaded circuit from API");
                        }
                        self.circuits.insert(id.to_string(), circuit);
                    }
                    Err(e) => {
                        warn!(id = id, error = %e, "failed to cache circuit");
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

        if let Some(circuit) = self.circuits.get(circuit_id) {
            return Ok(circuit.clone());
        }

        info!(id = circuit_id, "circuit not loaded, fetching from API");
        let url = format!("{}/circuits/{}", self.api_url, circuit_id);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("fetching circuit from API")?;

        if !resp.status().is_success() {
            anyhow::bail!("API returned {} for circuit {}", resp.status(), circuit_id);
        }

        let data: serde_json::Value = resp.json().await.context("parsing circuit response")?;
        let circuit = self.cache_and_load_circuit(circuit_id, &data).await?;
        self.circuits
            .insert(circuit_id.to_string(), circuit.clone());
        Ok(circuit)
    }

    pub async fn refresh_circuits(&mut self) -> Result<Vec<String>> {
        let api_circuits = self.fetch_circuits_from_api().await?;
        let active_ids: std::collections::HashSet<String> = api_circuits
            .iter()
            .filter_map(|c| c.get("id").and_then(|v| v.as_str()).map(String::from))
            .collect();

        for circuit_data in &api_circuits {
            if let Some(id) = circuit_data.get("id").and_then(|v| v.as_str()) {
                if self.circuits.contains_key(id) || IGNORED_MODEL_HASHES.contains(&id) {
                    continue;
                }
                match self.cache_and_load_circuit(id, circuit_data).await {
                    Ok(circuit) => {
                        info!(id = id, name = %circuit.metadata.name, "loaded new circuit");
                        self.circuits.insert(id.to_string(), circuit);
                    }
                    Err(e) => {
                        warn!(id = id, error = %e, "failed to load new circuit");
                    }
                }
            }
        }

        if active_ids.is_empty() {
            warn!("circuit API returned empty active set, skipping removal");
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

    pub const REFRESH_INTERVAL: u64 = REFRESH_INTERVAL_SECS;

    async fn fetch_circuits_from_api(&self) -> Result<Vec<serde_json::Value>> {
        let url = format!("{}/circuits", self.api_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("fetching circuits list")?;

        if !resp.status().is_success() {
            anyhow::bail!("API returned {}", resp.status());
        }

        let data: serde_json::Value = resp.json().await.context("parsing circuits response")?;
        let circuits = data
            .get("circuits")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(circuits)
    }

    async fn cache_and_load_circuit(
        &self,
        circuit_id: &str,
        data: &serde_json::Value,
    ) -> Result<Circuit> {
        let cache_path = self.cache_dir.join(format!("model_{circuit_id}"));
        std::fs::create_dir_all(&cache_path)
            .with_context(|| format!("creating cache dir {}", cache_path.display()))?;

        let metadata_value = data
            .get("metadata")
            .context("circuit data missing metadata")?;

        let metadata: CircuitMetadata =
            serde_json::from_value(metadata_value.clone()).context("parsing circuit metadata")?;

        let metadata_path = cache_path.join(CIRCUIT_METADATA_FILENAME);
        std::fs::write(
            &metadata_path,
            serde_json::to_string_pretty(metadata_value)?,
        )
        .context("writing metadata")?;

        let is_dsperse = metadata.circuit_type == CircuitType::DSPERSE_PROOF_GENERATION;

        if let Some(files) = data.get("files").and_then(|v| v.as_object()) {
            if is_dsperse {
                let slices_dir = cache_path.join("slices");
                std::fs::create_dir_all(&slices_dir).ok();
            }

            let mut deferred_downloads: Vec<(String, PathBuf)> = Vec::new();

            for (filename, url_val) in files {
                let skip = if is_dsperse {
                    filename == "full_model.onnx"
                } else {
                    SKIP_AUTO_DOWNLOAD.contains(&filename.as_str())
                };
                if skip {
                    continue;
                }

                if is_dsperse && filename.ends_with(".dslice") {
                    let archive_dest = cache_path.join("slices").join(filename);
                    let slice_name = filename.trim_end_matches(".dslice");
                    let extracted_dir = cache_path.join("slices").join(slice_name);
                    if archive_dest.exists() || extracted_dir.exists() {
                        continue;
                    }
                    if let Some(url) = url_val.as_str() {
                        deferred_downloads.push((url.to_string(), archive_dest));
                    }
                    continue;
                }

                let dest = if is_dsperse
                    && (filename == "metadata.json" || filename == "metadata.msgpack")
                {
                    cache_path.join("slices").join(filename)
                } else {
                    cache_path.join(filename)
                };
                if dest.exists() {
                    continue;
                }
                if let Some(url) = url_val.as_str() {
                    if let Err(e) = self.download_file(url, &dest).await {
                        warn!(file = %filename, error = %e, "failed to download circuit file");
                    }
                }
            }

            if !deferred_downloads.is_empty() {
                let count = deferred_downloads.len();
                let http = self.http.clone();
                info!(circuit = %circuit_id, files = count, "spawning background dslice downloads");
                tokio::spawn(async move {
                    let mut downloaded = 0usize;
                    for (url, dest) in &deferred_downloads {
                        if dest.exists() {
                            downloaded += 1;
                            continue;
                        }
                        let partial = dest.with_extension("dslice.partial");
                        match download_file_static(&http, url, &partial).await {
                            Ok(()) => {
                                if let Err(e) = std::fs::rename(&partial, dest) {
                                    warn!(
                                        file = %dest.display(),
                                        error = %e,
                                        "failed to rename partial dslice download"
                                    );
                                    std::fs::remove_file(&partial).ok();
                                    continue;
                                }
                                downloaded += 1;
                                if downloaded % 20 == 0 || downloaded == count {
                                    info!(progress = %format!("{downloaded}/{count}"), "dslice download progress");
                                }
                            }
                            Err(e) => {
                                std::fs::remove_file(&partial).ok();
                                warn!(file = %dest.display(), error = %e, "failed to download dslice file");
                            }
                        }
                    }
                    info!(count = downloaded, "dslice background downloads complete");
                });
            }
        }

        let settings = load_settings(&cache_path);
        let proof_system = parse_proof_system(&metadata.proof_system);

        Ok(Circuit {
            id: circuit_id.to_string(),
            paths: CircuitPaths::new(
                &format!("model_{circuit_id}"),
                &self.cache_dir.to_string_lossy(),
            ),
            metadata,
            proof_system,
            settings,
            timeout: CIRCUIT_TIMEOUT_SECONDS as f64,
        })
    }

    fn load_from_cache(&mut self, active_ids: &std::collections::HashSet<String>) {
        let cache_dir = &self.cache_dir;
        let entries = match std::fs::read_dir(cache_dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let dir_name = entry.file_name().to_string_lossy().to_string();
            let circuit_id = match dir_name.strip_prefix("model_") {
                Some(id) if id.len() == 64 => id.to_string(),
                _ => continue,
            };

            if !active_ids.is_empty() && !active_ids.contains(&circuit_id) {
                continue;
            }
            if self.circuits.contains_key(&circuit_id) {
                continue;
            }

            let metadata_path = entry.path().join(CIRCUIT_METADATA_FILENAME);
            if !metadata_path.exists() {
                continue;
            }

            match load_circuit_from_cache(&circuit_id, &entry.path(), &self.cache_dir) {
                Ok(circuit) => {
                    if circuit.metadata.circuit_type == CircuitType::DSPERSE_PROOF_GENERATION {
                        migrate_dslice_layout(&entry.path());
                    }
                    self.circuits.insert(circuit_id, circuit);
                }
                Err(e) => {
                    warn!(id = circuit_id, error = %e, "failed to load cached circuit");
                }
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

    let bytes = resp.bytes().await.context("reading download body")?;
    std::fs::write(dest, &bytes).with_context(|| format!("writing {}", dest.display()))?;

    Ok(())
}

fn load_circuit_from_cache(circuit_id: &str, dir: &Path, cache_dir: &Path) -> Result<Circuit> {
    let metadata_path = dir.join(CIRCUIT_METADATA_FILENAME);
    let metadata_str = std::fs::read_to_string(&metadata_path).context("reading metadata")?;
    let metadata: CircuitMetadata =
        serde_json::from_str(&metadata_str).context("parsing cached metadata")?;
    let settings = load_settings(dir);
    let proof_system = parse_proof_system(&metadata.proof_system);

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
    let tmp_dir = slices_dir.join(format!(".{slice_id}.extracting"));
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir).ok();
    }
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
        return Err(anyhow::anyhow!(
            "renaming {} to {}: {e}",
            tmp_dir.display(),
            extract_dir.display()
        ));
    }
    if let Err(e) = std::fs::remove_file(&archive) {
        tracing::warn!(
            archive = %archive.display(),
            error = %e,
            "failed to remove dslice archive after extraction"
        );
    }
    Ok(())
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

fn parse_proof_system(s: &str) -> ProofSystem {
    match s {
        "ZKML" => ProofSystem::ZKML,
        "CIRCOM" => ProofSystem::CIRCOM,
        "JOLT" => ProofSystem::JOLT,
        "EZKL" => ProofSystem::EZKL,
        "JSTPROVE" => ProofSystem::JSTPROVE,
        _ => ProofSystem::JSTPROVE,
    }
}
