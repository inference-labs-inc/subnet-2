use std::path::Path;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};

const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);
const API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const RELEASES_URL: &str =
    "https://api.github.com/repos/inference-labs-inc/subnet-2/releases/latest";

fn platform_suffix() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x86_64",
        ("macos", "aarch64") => "macos-aarch64",
        _ => "",
    }
}

#[derive(serde::Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(serde::Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

fn current_version() -> Result<semver::Version> {
    let raw = env!("CARGO_PKG_VERSION");
    semver::Version::parse(raw).with_context(|| format!("parsing compiled-in version '{raw}'"))
}

fn parse_tag(tag: &str) -> Result<semver::Version> {
    let stripped = tag.strip_prefix('v').unwrap_or(tag);
    semver::Version::parse(stripped).with_context(|| format!("parsing release tag '{tag}'"))
}

async fn check_and_update(
    client: &reqwest::Client,
    binary_name: &str,
    suffix: &str,
) -> Result<bool> {
    let release: Release = client
        .get(RELEASES_URL)
        .timeout(API_TIMEOUT)
        .header("User-Agent", "sn2-auto-update")
        .send()
        .await
        .context("fetching latest release")?
        .error_for_status()
        .context("GitHub API error")?
        .json()
        .await
        .context("parsing release JSON")?;

    let remote = parse_tag(&release.tag_name)?;
    let local = current_version()?;

    if remote <= local {
        return Ok(false);
    }

    info!(
        from = %local,
        to = %remote,
        "new version available, updating"
    );

    let asset_name = format!("{binary_name}-{suffix}");
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .with_context(|| format!("binary asset '{asset_name}' not found in release"))?;

    let checksums_asset = release
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS")
        .context("SHA256SUMS asset not found in release")?;

    let checksums_text = client
        .get(&checksums_asset.browser_download_url)
        .timeout(API_TIMEOUT)
        .header("User-Agent", "sn2-auto-update")
        .send()
        .await
        .context("downloading SHA256SUMS")?
        .error_for_status()?
        .text()
        .await
        .context("reading SHA256SUMS")?;

    let expected_hash = checksums_text
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let hash = parts.next()?;
            let name = parts.next()?;
            if name == asset_name {
                Some(hash.to_string())
            } else {
                None
            }
        })
        .with_context(|| format!("hash for '{asset_name}' not found in SHA256SUMS"))?;

    let binary_bytes = client
        .get(&asset.browser_download_url)
        .timeout(DOWNLOAD_TIMEOUT)
        .header("User-Agent", "sn2-auto-update")
        .send()
        .await
        .context("downloading binary")?
        .error_for_status()?
        .bytes()
        .await
        .context("reading binary bytes")?;

    let actual_hash = hex::encode(Sha256::digest(&binary_bytes));
    if actual_hash != expected_hash {
        bail!("SHA256 mismatch: expected {expected_hash}, got {actual_hash}");
    }

    let current_exe = std::env::current_exe().context("resolving current executable path")?;
    let parent = current_exe
        .parent()
        .context("resolving executable parent directory")?;
    let tmp_path = parent.join(format!(".{binary_name}.update.tmp"));

    std::fs::write(&tmp_path, &binary_bytes).context("writing temporary binary")?;

    if let Err(e) = set_executable(&tmp_path)
        .and_then(|()| std::fs::rename(&tmp_path, &current_exe).context("replacing current binary"))
    {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    info!(version = %remote, "update applied, exiting for restart");
    Ok(true)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(path, perms).context("setting executable permissions")
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

pub fn spawn_update_loop(binary_name: &'static str) -> tokio::task::JoinHandle<()> {
    let suffix = platform_suffix();
    if suffix.is_empty() {
        warn!("unsupported platform for auto-update, skipping");
        return tokio::spawn(std::future::ready(()));
    }

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CHECK_INTERVAL);
        interval.tick().await;
        loop {
            interval.tick().await;
            let client = match reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    error!(error = %e, "building HTTP client for auto-update, retrying next interval");
                    continue;
                }
            };
            match check_and_update(&client, binary_name, suffix).await {
                Ok(true) => std::process::exit(0),
                Ok(false) => {}
                Err(e) => warn!(error = %e, "auto-update check failed"),
            }
        }
    })
}
