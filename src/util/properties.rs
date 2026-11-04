use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

pub fn write_properties_atomic(path: &Path, props: &BTreeMap<String, String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create properties parent directory")?;
    }

    let tmp_path = path.with_extension("tmp");
    let mut payload = String::new();
    for (key, value) in props {
        payload.push_str(key);
        payload.push('=');
        payload.push_str(value);
        payload.push('\n');
    }

    std::fs::write(&tmp_path, payload).with_context(|| {
        format!(
            "Failed to write temp properties file: {}",
            tmp_path.display()
        )
    })?;
    std::fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "Failed to move temp properties into place: {} -> {}",
            tmp_path.display(),
            path.display()
        )
    })?;

    Ok(())
}
