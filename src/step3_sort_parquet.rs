use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;

use digested_rust::util::logging::init_dual_logging;
use digested_rust::util::result_json::write_json_atomic;

#[allow(non_snake_case)]
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Build directory base path used to resolve relative paths.
    #[arg(long, env = "PATH_BUILD_DIR")]
    PATH_BUILD_DIR: String,

    /// Log directory path. Usually set by main.py.
    #[arg(long, env = "PATH_LOG_DIR")]
    PATH_LOG_DIR: String,

    /// Result JSON file path for orchestrator. Usually set by main.py.
    #[arg(long, env = "PATH_RESULT_JSON_FILE")]
    PATH_RESULT_JSON_FILE: String,

    /// Input unsorted peptides parquet file path. Relative path is resolved against PATH_BUILD_DIR.
    #[arg(long, env = "PATH_PEPTIDES_UNSORTED_PARQUET_FILE")]
    PATH_PEPTIDES_UNSORTED_PARQUET_FILE: String,

    /// Output sorted peptides parquet file path. Relative path is resolved against PATH_BUILD_DIR.
    #[arg(long, env = "PATH_PEPTIDES_SORTED_PARQUET_FILE")]
    PATH_PEPTIDES_SORTED_PARQUET_FILE: String,

    /// DuckDB executable path/name.
    #[arg(long, env = "PATH_DUCKDB_BIN", default_value = "duckdb")]
    PATH_DUCKDB_BIN: String,

    /// Temporary directory for DuckDB spills. Relative path is resolved against PATH_BUILD_DIR.
    #[arg(long, env = "PATH_DUCKDB_TEMP_DIR", default_value = "intermediate_data/tmp_duckdb")]
    PATH_DUCKDB_TEMP_DIR: String,

    /// DuckDB memory limit (example: 90GB). Empty value means DuckDB default.
    #[arg(long, env = "DUCKDB_MEMORY_LIMIT")]
    DUCKDB_MEMORY_LIMIT: Option<String>,

    /// CRITICAL parameter: parquet row-group size for sorted output.
    /// Too small values may create too many row-groups and break downstream readers.
    #[arg(long, env = "STEP3_ROW_GROUP_SIZE", default_value_t = 5_000_000)]
    STEP3_ROW_GROUP_SIZE: usize,
}

#[derive(Serialize)]
struct Step3Result {
    sorted_output_file: String,
    row_group_size: usize,
    sort_order: String,
    _manifest: BTreeMap<String, serde_json::Value>,
}

fn sql_quote(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "''")
}

fn main() -> Result<()> {
    let args = Args::parse();

    let build_dir = PathBuf::from(&args.PATH_BUILD_DIR);
    let log_dir = PathBuf::from(&args.PATH_LOG_DIR);
    let input_path = build_dir.join(&args.PATH_PEPTIDES_UNSORTED_PARQUET_FILE);
    let output_path = build_dir.join(&args.PATH_PEPTIDES_SORTED_PARQUET_FILE);
    let temp_dir = build_dir.join(&args.PATH_DUCKDB_TEMP_DIR);

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("Failed to create output parent dir: {}", parent.display())
        })?;
    }
    std::fs::create_dir_all(&temp_dir)
        .with_context(|| format!("Failed to create temp dir: {}", temp_dir.display()))?;

    let log_file_name = std::env::current_exe()
        .ok()
        .and_then(|path| path.file_stem().map(|name| format!("{}.log", name.to_string_lossy())))
        .unwrap_or_else(|| "step3_sort_parquet.log".to_string());
    let _log_guard = init_dual_logging(&log_dir, &log_file_name)?;

    tracing::info!("Sorting peptides parquet with DuckDB...");
    tracing::info!("Input: {}", input_path.display());
    tracing::info!("Output: {}", output_path.display());
    tracing::info!("DuckDB temp dir: {}", temp_dir.display());
    tracing::info!("STEP3_ROW_GROUP_SIZE={}", args.STEP3_ROW_GROUP_SIZE);
    tracing::warn!(
        "STEP3_ROW_GROUP_SIZE is critical for downstream RocksDB reading stability; keep it large enough"
    );

    let mut sql_parts = Vec::new();
    if let Some(mem_limit) = args.DUCKDB_MEMORY_LIMIT.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        sql_parts.push(format!("SET memory_limit='{}';", mem_limit.replace('\'', "''")));
    }
    sql_parts.push(format!("SET temp_directory='{}';", sql_quote(&temp_dir)));
    sql_parts.push(format!(
        "COPY (SELECT * FROM '{}' ORDER BY mass, peptide) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD, ROW_GROUP_SIZE {});",
        sql_quote(&input_path),
        sql_quote(&output_path),
        args.STEP3_ROW_GROUP_SIZE,
    ));
    let sql = sql_parts.join("\n");

    let status = Command::new(&args.PATH_DUCKDB_BIN)
        .arg("-c")
        .arg(&sql)
        .status()
        .with_context(|| format!("Failed to execute DuckDB binary: {}", args.PATH_DUCKDB_BIN))?;

    if !status.success() {
        anyhow::bail!("DuckDB sort failed with exit status: {}", status);
    }

    let result = Step3Result {
        sorted_output_file: output_path.to_string_lossy().to_string(),
        row_group_size: args.STEP3_ROW_GROUP_SIZE,
        sort_order: "mass, peptide".to_string(),
        _manifest: BTreeMap::new(),
    };

    let result_path = Path::new(&args.PATH_RESULT_JSON_FILE);
    write_json_atomic(result_path, &result)
        .with_context(|| format!("Failed to write result json to: {}", args.PATH_RESULT_JSON_FILE))?;

    tracing::info!("Step3 sort completed successfully.");
    Ok(())
}
