use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use arrow_array::{Array, UInt32Array, UInt64Array};
use clap::Parser;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde::Serialize;

use digested_rust::util::logging::init_dual_logging;
use digested_rust::util::result_json::write_json_atomic;

#[allow(non_snake_case)]
#[derive(Parser, Debug)]
#[command(name = "step6_build_dict_bin", version, about)]
struct Args {
    /// Build directory base path used to resolve relative paths.
    #[arg(long, env = "PATH_BUILD_DIR")]
    PATH_BUILD_DIR: PathBuf,

    /// Log directory path. Usually set by main.py.
    #[arg(long, env = "PATH_LOG_DIR")]
    PATH_LOG_DIR: PathBuf,

    /// Result JSON file path for orchestrator. Usually set by main.py.
    #[arg(long, env = "PATH_RESULT_JSON_FILE")]
    PATH_RESULT_JSON_FILE: PathBuf,

    /// Input accession dictionary parquet file path. Relative path is resolved against PATH_BUILD_DIR.
    #[arg(long, env = "PATH_ACCESSION_DICT_PARQUET_FILE")]
    PATH_ACCESSION_DICT_PARQUET_FILE: PathBuf,

    /// Output binary dictionary file path. If omitted, defaults to PATH_BUILD_DIR/PATH_ROCKSDB_OUTPUT_DIR/acc_dict.bin.
    #[arg(long, env = "PATH_ACCESSION_DICT_BIN_FILE")]
    PATH_ACCESSION_DICT_BIN_FILE: Option<PathBuf>,

    /// RocksDB output directory relative to PATH_BUILD_DIR (used for default output path).
    #[arg(long, env = "PATH_ROCKSDB_OUTPUT_DIR")]
    PATH_ROCKSDB_OUTPUT_DIR: Option<PathBuf>,

    /// Parquet read batch size.
    #[arg(long, env = "STEP6_READ_BATCH_SIZE", default_value_t = 262_144)]
    STEP6_READ_BATCH_SIZE: usize,
}

#[derive(Serialize)]
struct Step6Result {
    dict_bin_file: String,
    entries_count: usize,
    max_accession_id: usize,
    file_size_bytes: u64,
    _manifest: BTreeMap<String, serde_json::Value>,
}

fn resolve_path(base: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

fn resolve_output_path(args: &Args) -> Result<PathBuf> {
    if let Some(p) = &args.PATH_ACCESSION_DICT_BIN_FILE {
        return Ok(resolve_path(&args.PATH_BUILD_DIR, p));
    }

    let rocks_dir = args
        .PATH_ROCKSDB_OUTPUT_DIR
        .as_ref()
        .context("Missing PATH_ROCKSDB_OUTPUT_DIR for default acc_dict.bin output path")?;
    Ok(args.PATH_BUILD_DIR.join(rocks_dir).join("acc_dict.bin"))
}

fn main() -> Result<()> {
    let args = Args::parse();
    let log_file_name = std::env::current_exe()
        .ok()
        .and_then(|path| path.file_stem().map(|name| format!("{}.log", name.to_string_lossy())))
        .unwrap_or_else(|| "step6_build_dict_bin.log".to_string());
    let _log_guard = init_dual_logging(&args.PATH_LOG_DIR, &log_file_name)?;

    let dict_parquet_path = resolve_path(&args.PATH_BUILD_DIR, &args.PATH_ACCESSION_DICT_PARQUET_FILE);
    let output_path = resolve_output_path(&args)?;

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create output directory: {}", parent.display()))?;
    }

    tracing::info!("Building accession dictionary binary...");
    tracing::info!("Input parquet: {}", dict_parquet_path.display());
    tracing::info!("Output bin: {}", output_path.display());
    tracing::info!("STEP6_READ_BATCH_SIZE={}", args.STEP6_READ_BATCH_SIZE);

    let file = File::open(&dict_parquet_path)
        .with_context(|| format!("Failed to open dict parquet file: {}", dict_parquet_path.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?
        .with_batch_size(args.STEP6_READ_BATCH_SIZE);

    let mut flat_array: Vec<u64> = vec![0; 16_000_000];
    let reader = builder.build()?;
    let mut max_id = 0usize;
    let mut entries_count = 0usize;

    for batch_result in reader {
        let batch = batch_result?;
        let id_col = batch
            .column_by_name("accession_id")
            .context("Missing column 'accession_id'")?;
        let long_col = batch
            .column_by_name("accession_long")
            .context("Missing column 'accession_long'")?;

        let ids = id_col
            .as_any()
            .downcast_ref::<UInt32Array>()
            .context("Column 'accession_id' has unexpected type")?;
        let longs = long_col
            .as_any()
            .downcast_ref::<UInt64Array>()
            .context("Column 'accession_long' has unexpected type")?;

        for i in 0..batch.num_rows() {
            if ids.is_null(i) || longs.is_null(i) {
                continue;
            }

            let id = ids.value(i) as usize;
            let long_val = longs.value(i);

            if id >= flat_array.len() {
                flat_array.resize(id + 10_000_000, 0);
            }
            flat_array[id] = long_val;
            entries_count += 1;

            if id > max_id {
                max_id = id;
            }
        }
    }

    flat_array.truncate(max_id.saturating_add(1));
    tracing::info!("Max accession_id: {}", max_id);
    tracing::info!("Entries written: {}", entries_count);

    let mut out_file = BufWriter::new(
        File::create(&output_path)
            .with_context(|| format!("Failed to create output file: {}", output_path.display()))?,
    );
    let byte_slice: &[u8] = unsafe {
        std::slice::from_raw_parts(
            flat_array.as_ptr() as *const u8,
            flat_array.len() * std::mem::size_of::<u64>(),
        )
    };
    out_file.write_all(byte_slice)?;
    out_file.flush()?;

    let file_size_bytes = std::fs::metadata(&output_path)?.len();
    tracing::info!("Dictionary binary size: {} bytes", file_size_bytes);

    let result = Step6Result {
        dict_bin_file: output_path.to_string_lossy().to_string(),
        entries_count,
        max_accession_id: max_id,
        file_size_bytes,
        _manifest: BTreeMap::from([
            (
                "accession_dict.file".to_string(),
                serde_json::json!(
                    output_path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default()
                ),
            ),
            (
                "accession_dict.entries_count".to_string(),
                serde_json::json!(entries_count),
            ),
            (
                "accession_dict.max_accession_id".to_string(),
                serde_json::json!(max_id),
            ),
        ]),
    };

    write_json_atomic(&args.PATH_RESULT_JSON_FILE, &result).with_context(|| {
        format!(
            "Failed to write result json to: {}",
            args.PATH_RESULT_JSON_FILE.display()
        )
    })?;

    Ok(())
}
