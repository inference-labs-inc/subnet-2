use std::path::Path;

use anyhow::{Context, Result};

pub fn atomic_write_json(path: &Path, data: &[u8]) -> Result<()> {
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, data)
        .with_context(|| format!("writing to {}", tmp_path.display()))?;
    std::fs::File::open(&tmp_path)
        .and_then(|f| f.sync_all())
        .with_context(|| format!("syncing {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("renaming {} to {}", tmp_path.display(), path.display()))?;
    Ok(())
}
