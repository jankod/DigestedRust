use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use arrow_array::{Array, Int32Array, RecordBatch, StringArray, UInt32Array};
use clap::Parser;
use humantime::format_duration;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use rkyv::api::high::to_bytes_in;
use rkyv::rancor::Error;
use rkyv::util::AlignedVec;
use rkyv::{Archive, Deserialize, Serialize};
use rocksdb::{DBCompactionStyle, DBCompressionType, Env, Options, SstFileWriter};
use serde::Serialize as SerdeSerialize;

use digested_rust::util::logging::init_dual_logging;
use digested_rust::util::result_json::write_json_atomic;

const DEFAULT_READ_BATCH_SIZE: usize = 1_000_000;
const DEFAULT_MAX_SST_FILE_SIZE_GB: usize = 2;

/// WRITE_FAST profile: write buffer size (memtable) in MB.
const WRITE_FAST_WRITE_BUFFER_MB: usize = 256;
/// WRITE_FAST profile: keep all table files open (recommended on Linux servers).
const WRITE_FAST_MAX_OPEN_FILES: i32 = -1;
/// WRITE_FAST profile: use Snappy for regular levels.
const WRITE_FAST_COMPRESSION: DBCompressionType = DBCompressionType::Snappy;
/// WRITE_FAST profile: use Zstd for bottommost level to save final disk space.
const WRITE_FAST_BOTTOMMOST_COMPRESSION: DBCompressionType = DBCompressionType::Zstd;

fn build_write_fast_options(write_buffer_mb: usize) -> Result<Options> {
    let write_buffer_bytes = write_buffer_mb
        .checked_mul(1024 * 1024)
        .context("STEP4_WRITE_FAST_WRITE_BUFFER_MB too large")?;

    let mut options = Options::default();
    options.create_if_missing(true);
    options.set_compaction_style(DBCompactionStyle::Level);
    options.set_write_buffer_size(write_buffer_bytes);
    options.set_compression_type(WRITE_FAST_COMPRESSION);
    options.set_bottommost_compression_type(WRITE_FAST_BOTTOMMOST_COMPRESSION);
    options.set_level_compaction_dynamic_level_bytes(true);
    options.set_max_open_files(WRITE_FAST_MAX_OPEN_FILES);
    options.set_allow_mmap_reads(false);
    options.set_allow_mmap_writes(false);

    let parallelism = std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(4)
        .max(2);
    options.increase_parallelism(parallelism);
    options.set_max_background_jobs(parallelism);

    let env = Env::new()?;
    options.set_env(&env);
    Ok(options)
}

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

    /// Input sorted peptides parquet file path. Relative path is resolved against PATH_BUILD_DIR.
    #[arg(long, env = "PATH_PEPTIDES_SORTED_PARQUET_FILE")]
    PATH_PEPTIDES_SORTED_PARQUET_FILE: String,

    /// Output RocksDB directory path. Relative path is resolved against PATH_BUILD_DIR.
    #[arg(long, env = "PATH_ROCKSDB_OUTPUT_DIR")]
    PATH_ROCKSDB_OUTPUT_DIR: String,

    /// Input parquet read batch size.
    #[arg(long, env = "STEP4_READ_BATCH_SIZE", default_value_t = DEFAULT_READ_BATCH_SIZE)]
    STEP4_READ_BATCH_SIZE: usize,

    /// Max SST file size in GB before rolling to the next SST part.
    #[arg(long, env = "STEP4_MAX_SST_FILE_SIZE_GB", default_value_t = DEFAULT_MAX_SST_FILE_SIZE_GB)]
    STEP4_MAX_SST_FILE_SIZE_GB: usize,

    /// WRITE_FAST profile write buffer size in MB (memtable size).
    #[arg(long, env = "STEP4_WRITE_FAST_WRITE_BUFFER_MB", default_value_t = WRITE_FAST_WRITE_BUFFER_MB)]
    STEP4_WRITE_FAST_WRITE_BUFFER_MB: usize,
}

#[derive(Archive, Deserialize, Serialize, Debug, PartialEq)]
#[rkyv(compare(PartialEq))]
#[rkyv(derive(Debug))]
pub struct PeptideRecord {
    pub peptide: String,
    pub accession_id: u32,
    pub tax_id: i32,
}

#[derive(Archive, Deserialize, Serialize, Debug, PartialEq, Default)]
#[rkyv(compare(PartialEq))]
#[rkyv(derive(Debug))]
pub struct MassGroup {
    pub records: Vec<PeptideRecord>,
}

#[derive(SerdeSerialize)]
struct Step4Result {
    sst_parts: usize,
    mass_keys_total: u64,
    peptide_records_total: u64,
    rocksdb_path: String,
    _manifest: BTreeMap<String, serde_json::Value>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let build_dir = PathBuf::from(&args.PATH_BUILD_DIR);
    let log_dir_path = PathBuf::from(&args.PATH_LOG_DIR);
    let input_path = build_dir.join(&args.PATH_PEPTIDES_SORTED_PARQUET_FILE);
    let db_path = build_dir.join(&args.PATH_ROCKSDB_OUTPUT_DIR);

    let max_sst_file_size = args
        .STEP4_MAX_SST_FILE_SIZE_GB
        .checked_mul(1024 * 1024 * 1024)
        .context("STEP4_MAX_SST_FILE_SIZE_GB too large")?;
    if args.STEP4_WRITE_FAST_WRITE_BUFFER_MB == 0 {
        anyhow::bail!("STEP4_WRITE_FAST_WRITE_BUFFER_MB must be >= 1");
    }

    let log_file_name = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_stem()
                .map(|name| format!("{}.log", name.to_string_lossy()))
        })
        .unwrap_or_else(|| "step4_populate_rocks_db.log".to_string());
    let _log_guard = init_dual_logging(&log_dir_path, &log_file_name)?;

    tracing::info!("Populating RocksDB from sorted parquet...");
    tracing::info!("Input sorted parquet: {}", input_path.display());
    tracing::info!("Output RocksDB directory: {}", db_path.display());
    tracing::info!("STEP4_READ_BATCH_SIZE={}", args.STEP4_READ_BATCH_SIZE);
    tracing::info!(
        "STEP4_MAX_SST_FILE_SIZE_GB={}",
        args.STEP4_MAX_SST_FILE_SIZE_GB
    );
    tracing::info!(
        "STEP4_WRITE_FAST_WRITE_BUFFER_MB={}",
        args.STEP4_WRITE_FAST_WRITE_BUFFER_MB
    );

    let sst_dir = db_path.join("sst_temp");
    if sst_dir.exists() {
        fs::remove_dir_all(&sst_dir).context("Failed to delete old sst dir")?;
    }
    fs::create_dir_all(&sst_dir).context("Failed to create sst dir")?;

    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create rocksdb parent directory: {}",
                parent.display()
            )
        })?;
    }

    let input_file = std::fs::File::open(&input_path).with_context(|| {
        format!(
            "Failed to open input parquet file: {}",
            input_path.display()
        )
    })?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(input_file)?
        .with_batch_size(args.STEP4_READ_BATCH_SIZE)
        .build()?;

    let options = build_write_fast_options(args.STEP4_WRITE_FAST_WRITE_BUFFER_MB)?;

    let mut current_sst_index = 0usize;
    let mut writer = create_sst_writer(&sst_dir, current_sst_index, &options)?;
    let mut current_sst_size = 0usize;

    let mut current_mass: Option<u32> = None;
    let mut current_group = MassGroup::default();

    let mut total_mass_keys = 0u64;
    let mut total_peptides = 0u64;
    let start = Instant::now();
    let mut file_paths = Vec::new();
    let mut buffer = AlignedVec::<16>::new();

    for batch_result in reader {
        let batch = batch_result?;
        let (masses, peptides, accessions, tax_ids) = get_columns(&batch)?;

        for i in 0..batch.num_rows() {
            if masses.is_null(i)
                || peptides.is_null(i)
                || accessions.is_null(i)
                || tax_ids.is_null(i)
            {
                continue;
            }

            let mass = masses.value(i);
            let peptide = peptides.value(i).to_string();
            let accession_id = accessions.value(i);
            let tax_id = tax_ids.value(i);

            if let Some(c_mass) = current_mass {
                if mass != c_mass {
                    let key_bytes = c_mass.to_be_bytes();
                    buffer.clear();
                    let serialized_value = to_bytes_in::<_, Error>(&current_group, &mut buffer)
                        .context("rkyv serialization failed")?;

                    writer.put(&key_bytes, &*serialized_value)?;
                    current_sst_size += key_bytes.len() + serialized_value.len();
                    total_mass_keys += 1;

                    if current_sst_size >= max_sst_file_size {
                        writer.finish()?;
                        file_paths.push(sst_file_path(&sst_dir, current_sst_index));
                        tracing::info!("Finished SST part {}", current_sst_index);

                        current_sst_index += 1;
                        writer = create_sst_writer(&sst_dir, current_sst_index, &options)?;
                        current_sst_size = 0;
                    }

                    current_group.records.clear();
                    current_mass = Some(mass);
                }
            } else {
                current_mass = Some(mass);
            }

            current_group.records.push(PeptideRecord {
                peptide,
                accession_id,
                tax_id,
            });
            total_peptides += 1;
        }
    }

    if let Some(c_mass) = current_mass {
        if !current_group.records.is_empty() {
            let key_bytes = c_mass.to_be_bytes();
            buffer.clear();
            let serialized_value = to_bytes_in::<_, Error>(&current_group, &mut buffer)
                .context("rkyv serialization failed")?;
            writer.put(&key_bytes, &*serialized_value)?;
            total_mass_keys += 1;
        }
    }

    writer.finish()?;
    file_paths.push(sst_file_path(&sst_dir, current_sst_index));
    tracing::info!(
        "All SST chunks generated in {}",
        format_duration(start.elapsed())
    );

    tracing::info!("Ingesting SST files into main DB...");
    let db = rocksdb::DB::open(&options, &db_path)
        .with_context(|| format!("Failed to open RocksDB: {}", db_path.display()))?;
    let mut ingest_options = rocksdb::IngestExternalFileOptions::default();
    ingest_options.set_move_files(true);
    ingest_options.set_snapshot_consistency(false);
    ingest_options.set_allow_global_seqno(false);

    let file_path_strings: Vec<String> = file_paths
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    db.ingest_external_file_opts(&ingest_options, file_path_strings)?;

    tracing::info!(
        "Insertion completed in {}.",
        format_duration(start.elapsed())
    );
    tracing::info!("Total mass keys (unique): {}", total_mass_keys);
    tracing::info!("Total peptide records: {}", total_peptides);

    let sst_parts = current_sst_index + 1;
    let result = Step4Result {
        sst_parts,
        mass_keys_total: total_mass_keys,
        peptide_records_total: total_peptides,
        rocksdb_path: db_path.to_string_lossy().to_string(),
        _manifest: BTreeMap::from([
            ("storage.engine".to_string(), serde_json::json!("rocksdb")),
            (
                "storage.path".to_string(),
                serde_json::json!(db_path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default()),
            ),
            (
                "storage.sst_parts".to_string(),
                serde_json::json!(sst_parts),
            ),
            (
                "storage.sst_max_file_size_gb".to_string(),
                serde_json::json!(args.STEP4_MAX_SST_FILE_SIZE_GB),
            ),
            (
                "statistics.mass_keys_total".to_string(),
                serde_json::json!(total_mass_keys),
            ),
            (
                "statistics.peptide_records_total".to_string(),
                serde_json::json!(total_peptides),
            ),
        ]),
    };

    let result_path = Path::new(&args.PATH_RESULT_JSON_FILE);
    write_json_atomic(result_path, &result).with_context(|| {
        format!(
            "Failed to write result json to: {}",
            args.PATH_RESULT_JSON_FILE
        )
    })?;

    Ok(())
}

fn create_sst_writer<'a>(
    dir: &Path,
    index: usize,
    options: &'a Options,
) -> Result<SstFileWriter<'a>> {
    let writer = SstFileWriter::create(options);
    let path = sst_file_path(dir, index);
    writer.open(path.to_str().unwrap())?;
    Ok(writer)
}

fn sst_file_path(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("part_{:05}.sst", index))
}

fn get_columns(
    batch: &RecordBatch,
) -> Result<(&UInt32Array, &StringArray, &UInt32Array, &Int32Array)> {
    let mass_col = batch
        .column_by_name("mass")
        .context("Missing 'mass' column in sorted input parquet")?;
    let peptide_col = batch
        .column_by_name("peptide")
        .context("Missing 'peptide' column")?;
    let accession_col = batch
        .column_by_name("accession_id")
        .context("Missing 'accession_id' column")?;
    let tax_id_col = batch
        .column_by_name("tax_id")
        .context("Missing 'tax_id' column")?;

    let masses = mass_col
        .as_any()
        .downcast_ref::<UInt32Array>()
        .context("'mass' column is not UInt32")?;
    let peptides = peptide_col
        .as_any()
        .downcast_ref::<StringArray>()
        .context("'peptide' column is not Utf8")?;
    let accessions = accession_col
        .as_any()
        .downcast_ref::<UInt32Array>()
        .context("'accession_id' column is not UInt32")?;
    let tax_ids = tax_id_col
        .as_any()
        .downcast_ref::<Int32Array>()
        .context("'tax_id' column is not Int32")?;

    Ok((masses, peptides, accessions, tax_ids))
}
