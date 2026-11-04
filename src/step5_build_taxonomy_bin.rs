use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use humantime::format_duration;
use serde::Serialize;

use digested_rust::taxonomy::db::TaxonomyDbBuilder;
use digested_rust::util::logging::init_dual_logging;
use digested_rust::util::result_json::write_json_atomic;

/// Build a compact `taxonomy.bin` from NCBI taxdump files.
#[allow(non_snake_case)]
#[derive(Parser, Debug)]
#[command(name = "step5_build_taxonomy_bin", version, about)]
struct Cli {
    /// Log directory path. Usually set by main.py.
    #[arg(long, env = "PATH_LOG_DIR")]
    PATH_LOG_DIR: PathBuf,

    /// Result JSON file path for orchestrator. Usually set by main.py.
    #[arg(long, env = "PATH_RESULT_JSON_FILE")]
    PATH_RESULT_JSON_FILE: PathBuf,

    /// Path to nodes.dmp
    #[arg(long, env = "PATH_TAXONOMY_NODES_DMP_FILE")]
    PATH_TAXONOMY_NODES_DMP_FILE: PathBuf,

    /// Path to names.dmp
    #[arg(long, env = "PATH_TAXONOMY_NAMES_DMP_FILE")]
    PATH_TAXONOMY_NAMES_DMP_FILE: PathBuf,

    /// Path to merged.dmp (optional — resolves old TaxIDs)
    #[arg(long, env = "PATH_TAXONOMY_MERGED_DMP_FILE")]
    PATH_TAXONOMY_MERGED_DMP_FILE: Option<PathBuf>,

    /// Output path for taxonomy.bin.
    /// If omitted, defaults to: PATH_BUILD_DIR/PATH_ROCKSDB_OUTPUT_DIR/taxonomy.bin
    #[arg(long, short, env = "PATH_TAXONOMY_BIN_FILE")]
    PATH_TAXONOMY_BIN_FILE: Option<PathBuf>,

    /// Base build directory (used only for resolving default output path)
    #[arg(long, env = "PATH_BUILD_DIR")]
    PATH_BUILD_DIR: Option<PathBuf>,

    /// RocksDB output directory relative to PATH_BUILD_DIR (used for default output path)
    #[arg(long, env = "PATH_ROCKSDB_OUTPUT_DIR")]
    PATH_ROCKSDB_OUTPUT_DIR: Option<PathBuf>,
}

#[derive(Serialize)]
struct Step5Result {
    taxonomy_bin_file: String,
    nodes_count: usize,
    names_count: usize,
    merged_count: usize,
    _manifest: BTreeMap<String, serde_json::Value>,
}

fn resolve_output_path(cli: &Cli) -> Result<PathBuf> {
    if let Some(path) = &cli.PATH_TAXONOMY_BIN_FILE {
        if path.is_absolute() {
            return Ok(path.clone());
        }
        if let Some(build_dir) = &cli.PATH_BUILD_DIR {
            return Ok(build_dir.join(path));
        }
        return Ok(path.clone());
    }

    let build_dir = cli
        .PATH_BUILD_DIR
        .as_ref()
        .context("Missing PATH_BUILD_DIR for default taxonomy output path")?;
    let rocks_dir = cli
        .PATH_ROCKSDB_OUTPUT_DIR
        .as_ref()
        .context("Missing PATH_ROCKSDB_OUTPUT_DIR for default taxonomy output path")?;

    Ok(build_dir.join(rocks_dir).join("taxonomy.bin"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let start = Instant::now();
    let log_file_name = std::env::current_exe()
        .ok()
        .and_then(|path| path.file_stem().map(|name| format!("{}.log", name.to_string_lossy())))
        .unwrap_or_else(|| "step5_build_taxonomy_bin.log".to_string());
    let _log_guard = init_dual_logging(&cli.PATH_LOG_DIR, &log_file_name)?;

    let output = resolve_output_path(&cli)?;
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create output directory: {}", parent.display()))?;
    }

    tracing::info!("Building taxonomy.bin...");
    tracing::info!("Nodes file: {}", cli.PATH_TAXONOMY_NODES_DMP_FILE.display());
    tracing::info!("Names file: {}", cli.PATH_TAXONOMY_NAMES_DMP_FILE.display());
    if let Some(ref m) = cli.PATH_TAXONOMY_MERGED_DMP_FILE {
        tracing::info!("Merged file: {}", m.display());
    }
    tracing::info!("Output file: {}", output.display());

    let mut builder = TaxonomyDbBuilder::new();

    let n_nodes = builder
        .parse_nodes(&cli.PATH_TAXONOMY_NODES_DMP_FILE)
        .context("Failed to parse nodes.dmp")?;
    tracing::info!("Parsed {} nodes", n_nodes);

    let n_names = builder
        .parse_names(&cli.PATH_TAXONOMY_NAMES_DMP_FILE)
        .context("Failed to parse names.dmp")?;
    tracing::info!("Parsed {} scientific names", n_names);

    let mut n_merged = 0usize;
    if let Some(ref merged_path) = cli.PATH_TAXONOMY_MERGED_DMP_FILE {
        n_merged = builder
            .parse_merged(merged_path)
            .context("Failed to parse merged.dmp")?;
        tracing::info!("Parsed {} merged entries", n_merged);
    }

    builder
        .write(&output)
        .context("Failed to write taxonomy.bin")?;

    let file_size = std::fs::metadata(&output)?.len();
    tracing::info!(
        "Build complete. Output: {}, Size: {:.1} MB, Time: {}",
        output.display(),
        file_size as f64 / 1_000_000.0,
        format_duration(start.elapsed())
    );

    let result = Step5Result {
        taxonomy_bin_file: output.to_string_lossy().to_string(),
        nodes_count: n_nodes,
        names_count: n_names,
        merged_count: n_merged,
        _manifest: BTreeMap::from([
            (
                "taxonomy.file".to_string(),
                serde_json::json!(output.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()),
            ),
            ("taxonomy.nodes_count".to_string(), serde_json::json!(n_nodes)),
            ("taxonomy.names_count".to_string(), serde_json::json!(n_names)),
            ("taxonomy.merged_count".to_string(), serde_json::json!(n_merged)),
        ]),
    };
    write_json_atomic(&cli.PATH_RESULT_JSON_FILE, &result).with_context(|| {
        format!(
            "Failed to write result json to: {}",
            cli.PATH_RESULT_JSON_FILE.display()
        )
    })?;

    Ok(())
}
