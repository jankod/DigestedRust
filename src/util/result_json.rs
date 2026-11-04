use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

/// Write JSON atomically by writing to a temp file and then renaming.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create result json parent directory")?;
    }

    let tmp_path = path.with_extension("tmp");
    let payload = serde_json::to_vec_pretty(value).context("Failed to serialize result json")?;

    std::fs::write(&tmp_path, payload).with_context(|| {
        format!(
            "Failed to write temp result json file: {}",
            tmp_path.display()
        )
    })?;

    std::fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "Failed to move temp result json into place: {} -> {}",
            tmp_path.display(),
            path.display()
        )
    })?;

    Ok(())
}
