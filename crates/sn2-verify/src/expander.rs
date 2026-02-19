use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

const BINARY_SEARCH_PATHS: &[&str] = &["./Expander/target/release/expander-exec"];

const TIMEOUT: Duration = Duration::from_secs(120);

fn find_expander_binary() -> Result<PathBuf> {
    if let Ok(val) = std::env::var("SN2_EXPANDER_EXEC") {
        let p = PathBuf::from(&val);
        if p.exists() {
            return Ok(p);
        }
        bail!("SN2_EXPANDER_EXEC={val} does not exist");
    }
    if let Ok(p) = which::which("expander-exec") {
        return Ok(p);
    }
    for path in BINARY_SEARCH_PATHS {
        let p = PathBuf::from(path);
        if p.exists() {
            return Ok(p);
        }
    }
    bail!("expander-exec binary not found in PATH or search paths (set SN2_EXPANDER_EXEC)")
}

pub async fn run_expander_verify(
    circuit_path: &Path,
    witness_path: &Path,
    proof_path: &Path,
    pcs_type: &str,
) -> Result<bool> {
    let binary = find_expander_binary()?;

    let child = Command::new(&binary)
        .args([
            "-p",
            pcs_type,
            "verify",
            "-c",
            circuit_path.to_str().context("circuit_path not utf8")?,
            "-w",
            witness_path.to_str().context("witness_path not utf8")?,
            "-i",
            proof_path.to_str().context("proof_path not utf8")?,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning {}", binary.display()))?;

    let output = tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .context("expander-exec timed out after 120s")?
        .context("waiting for expander-exec")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        tracing::warn!(
            exit_code = output.status.code(),
            stderr = %stderr,
            stdout = %stdout,
            "expander-exec verification failed"
        );
    }

    Ok(output.status.success())
}
