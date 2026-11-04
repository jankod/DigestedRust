use std::fs::File;
use std::io::{BufReader, Read};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use arrow_array::{ArrayRef, Int32Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use flate2::read::GzDecoder;
use humantime::format_duration;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;
use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;
use sha2::{Digest, Sha256};

use digested_rust::uniprot::xml_reader::{self, ProteinInfo};
use digested_rust::util::logging::init_dual_logging;
use digested_rust::util::result_json::write_json_atomic;


// TODO: exit code != 0 on error

/// Program for parsing UniProt XML files and converting them to Parquet format.
#[allow(non_snake_case)]
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Input XML.GZ file path. Relative path is resolved against PATH_BUILD_DIR.
    #[arg(long, env = "PATH_INPUT_XML_GZ_FILE")]
    PATH_INPUT_XML_GZ_FILE: String,

    /// Output proteins parquet file path. Relative path is resolved against PATH_BUILD_DIR.
    #[arg(long, env = "PATH_PROTEINS_PARQUET_FILE")]
    PATH_PROTEINS_PARQUET_FILE: String,

    /// Result JSON file path for orchestrator. Usually set by main.py.
    #[arg(long, env = "PATH_RESULT_JSON_FILE")]
    PATH_RESULT_JSON_FILE: String,

    /// Log directory path. Usually set by main.py.
    #[arg(long, env = "PATH_LOG_DIR")]
    PATH_LOG_DIR: String,

    /// Rows per parquet row group (integer; default 1_000_000).
    #[arg(long, env = "PARAM_STEP1_PARQUET_ROW_GROUP_ROWS", default_value_t = 1_000_000)]
    PARAM_STEP1_PARQUET_ROW_GROUP_ROWS: usize,

    /// Build directory base path used to resolve relative paths.
    #[arg(long, env = "PATH_BUILD_DIR")]
    PATH_BUILD_DIR: String,

    /// Optional CSV taxonomy filters (example: 9606,10090). Empty = no filter.
    #[arg(long, env = "MS_SOURCE__FILTER_TAX_IDS")]
    MS_SOURCE__FILTER_TAX_IDS: Option<String>,

    /// Optional CSV division filters (example: 0,2,7). Empty = no filter.
    #[arg(long, env = "MS_SOURCE__FILTER_DIV_IDS")]
    MS_SOURCE__FILTER_DIV_IDS: Option<String>,
}

#[derive(Serialize)]
struct Step1Result {
    proteins_total: u64,
    input_file_sha256: String,
    input_file_size_bytes: u64,
    _manifest: BTreeMap<String, serde_json::Value>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let build_dir = std::path::PathBuf::from(&args.PATH_BUILD_DIR);
    let log_dir_path = std::path::PathBuf::from(&args.PATH_LOG_DIR);
    
    // Resolve potentially relative parquet config path into an absolute output path
    let output_parquet_path = build_dir.join(&args.PATH_PROTEINS_PARQUET_FILE);

    let log_file_name = std::env::current_exe()
        .ok()
        .and_then(|path| path.file_stem().map(|name| format!("{}.log", name.to_string_lossy())))
        .unwrap_or_else(|| "step1_uniprot_xml_to_parquet.log".to_string());
    let _log_guard = init_dual_logging(&log_dir_path, &log_file_name)?;

    tracing::info!("Parsing XML and writing to Parquet...");
    tracing::info!("Input: {}", args.PATH_INPUT_XML_GZ_FILE);
    let tax_filters = parse_optional_id_set(args.MS_SOURCE__FILTER_TAX_IDS.as_deref())
        .context("Invalid MS_SOURCE__FILTER_TAX_IDS value")?;
    let div_filters = parse_optional_id_set(args.MS_SOURCE__FILTER_DIV_IDS.as_deref())
        .context("Invalid MS_SOURCE__FILTER_DIV_IDS value")?;

    if let Some(ids) = &tax_filters {
        tracing::info!("Taxonomy filters active ({} ids)", ids.len());
    }
    if let Some(ids) = &div_filters {
        tracing::info!("Division filters active ({} ids)", ids.len());
    }

    let input_sha256 = get_or_create_input_sha256_cache(&args.PATH_INPUT_XML_GZ_FILE)
        .with_context(|| format!("Failed to resolve SHA-256 for input file: {}", args.PATH_INPUT_XML_GZ_FILE))?;
    let input_file_size_bytes = std::fs::metadata(&args.PATH_INPUT_XML_GZ_FILE)
        .with_context(|| format!("Failed to read input file metadata: {}", args.PATH_INPUT_XML_GZ_FILE))?
        .len();
    tracing::info!("Input SHA-256: {}", input_sha256);

    let file = File::open(&args.PATH_INPUT_XML_GZ_FILE)
        .with_context(|| format!("Failed to open input file: {}", args.PATH_INPUT_XML_GZ_FILE))?;
    let decoder = GzDecoder::new(file);
    let buf_reader = BufReader::new(decoder);

    let schema = Arc::new(Schema::new(vec![
        Field::new("accession", DataType::Utf8, true),
        Field::new("protein_name", DataType::Utf8, true),
        Field::new("taxonomy_id", DataType::Int32, false),
        Field::new("division_id", DataType::Int32, false),
        Field::new("sequence", DataType::Utf8, true),
    ]));

    let parquet_file = File::create(&output_parquet_path)
        .with_context(|| format!("Failed to create output file: {}", output_parquet_path.display()))?;
    
    let props = WriterProperties::builder()
        .set_dictionary_enabled(true)
        .set_max_row_group_row_count(Some(args.PARAM_STEP1_PARQUET_ROW_GROUP_ROWS))
        .set_compression(parquet::basic::Compression::ZSTD(parquet::basic::ZstdLevel::try_new(3)?))
        .build();

    // .set_dictionary_enabled(true)
    // .set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?))
    // .set_max_row_group_size(256 * 1024 * 1024)

    let mut writer = ArrowWriter::try_new(parquet_file, schema.clone(), Some(props))?;

    let start = Instant::now();
    let batch_size = 10000;
    let mut batch = ProteinBatch::new();
    let mut selected_entry_count: u64 = 0;

    let entry_count = xml_reader::parse_proteins(buf_reader, |protein, entry_count| {
        if !protein_matches_filters(&protein, tax_filters.as_ref(), div_filters.as_ref()) {
            return Ok(());
        }

        selected_entry_count += 1;
        batch.add(protein);

        if batch.len() >= batch_size {
            let record_batch = batch.to_record_batch(schema.clone())?;
            writer.write(&record_batch)?;
            batch.clear();

            if entry_count % 50000 == 0 {
                tracing::info!("Processed {} entries...", entry_count);
            }
        }

        Ok(())
    })?;

    if !batch.is_empty() {
        let record_batch = batch.to_record_batch(schema.clone())?;
        writer.write(&record_batch)?;
    }

    writer.close()?;

    let elapsed = start.elapsed();
    tracing::info!("=== Parsing Complete ===");
    tracing::info!("Parsed entries (raw): {}", entry_count);
    tracing::info!("Parsed entries (selected): {}", selected_entry_count);
    tracing::info!("Parse time: {}", format_duration(elapsed));
    tracing::info!("Output file: {}", output_parquet_path.display());

    let result = Step1Result {
        proteins_total: selected_entry_count,
        input_file_sha256: input_sha256.clone(),
        input_file_size_bytes,
        _manifest: BTreeMap::from([
            ("source.entries".to_string(), serde_json::json!(selected_entry_count)),
            ("statistics.proteins_total".to_string(), serde_json::json!(selected_entry_count)),
            ("source.input_file_sha256".to_string(), serde_json::json!(input_sha256)),
            ("source.input_file_size_bytes".to_string(), serde_json::json!(input_file_size_bytes)),
        ]),
    };
    let result_path = std::path::Path::new(&args.PATH_RESULT_JSON_FILE);
    write_json_atomic(result_path, &result)
        .with_context(|| format!("Failed to write result json to: {}", args.PATH_RESULT_JSON_FILE))?;

    Ok(())
}

fn protein_matches_filters(
    protein: &ProteinInfo,
    tax_filters: Option<&HashSet<i32>>,
    div_filters: Option<&HashSet<i32>>,
) -> bool {
    let tax_ok = tax_filters
        .map(|set| set.contains(&protein.taxonomy_id))
        .unwrap_or(true);
    let div_ok = div_filters
        .map(|set| set.contains(&protein.division_id))
        .unwrap_or(true);
    tax_ok && div_ok
}

fn parse_optional_id_set(input: Option<&str>) -> Result<Option<HashSet<i32>>> {
    let Some(raw) = input else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let mut set = HashSet::new();
    for token in trimmed.split(',') {
        let value = token.trim();
        if value.is_empty() {
            continue;
        }
        let parsed = value
            .parse::<i32>()
            .with_context(|| format!("Could not parse '{value}' as i32"))?;
        set.insert(parsed);
    }

    if set.is_empty() {
        Ok(None)
    } else {
        Ok(Some(set))
    }
}

fn compute_file_sha256(path: &str) -> Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];

    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

fn get_or_create_input_sha256_cache(input_xml_path: &str) -> Result<String> {
    let input_path = std::path::Path::new(input_xml_path);
    let cache_dir = input_path.parent().unwrap_or(std::path::Path::new("."));
    let input_file_name = input_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("input");
    let cache_file = cache_dir.join(format!("{}.sha256", input_file_name));

    if cache_file.exists() {
        let cached = std::fs::read_to_string(&cache_file)
            .with_context(|| format!("Failed to read SHA-256 cache: {}", cache_file.display()))?;
        let hash = cached.trim();
        if !hash.is_empty() {
            tracing::info!("Using cached input SHA-256 from {}", cache_file.display());
            return Ok(hash.to_string());
        }
    }

    let hash = compute_file_sha256(input_xml_path)?;
    std::fs::write(&cache_file, format!("{hash}\n"))
        .with_context(|| format!("Failed to write SHA-256 cache file: {}", cache_file.display()))?;
    tracing::info!("Stored input SHA-256 cache at {}", cache_file.display());
    Ok(hash)
}



struct ProteinBatch {
    accessions: Vec<Option<String>>,
    protein_names: Vec<Option<String>>,
    taxonomy_ids: Vec<i32>,
    division_ids: Vec<i32>,
    sequences: Vec<Option<String>>,
}

impl ProteinBatch {
    fn new() -> Self {
        Self {
            accessions: Vec::with_capacity(10000),
            protein_names: Vec::with_capacity(10000),
            taxonomy_ids: Vec::with_capacity(10000),
            division_ids: Vec::with_capacity(10000),
            sequences: Vec::with_capacity(10000),
        }
    }

    fn add(&mut self, protein: ProteinInfo) {
        self.accessions.push(protein.accession);
        self.protein_names.push(protein.protein_name);
        self.taxonomy_ids.push(protein.taxonomy_id);
        self.division_ids.push(protein.division_id);
        self.sequences.push(protein.sequence);
    }

    fn len(&self) -> usize {
        self.accessions.len()
    }

    fn is_empty(&self) -> bool {
        self.accessions.is_empty()
    }

    fn clear(&mut self) {
        self.accessions.clear();
        self.protein_names.clear();
        self.taxonomy_ids.clear();
        self.division_ids.clear();
        self.sequences.clear();
    }

    fn to_record_batch(&self, schema: Arc<Schema>) -> anyhow::Result<RecordBatch> {
        let accession_array = StringArray::from(self.accessions.clone());
        let protein_name_array = StringArray::from(self.protein_names.clone());
        let taxonomy_id_array = Int32Array::from(self.taxonomy_ids.clone());
        let division_id_array = Int32Array::from(self.division_ids.clone());
        let sequence_array = StringArray::from(self.sequences.clone());

        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(accession_array) as ArrayRef,
                Arc::new(protein_name_array) as ArrayRef,
                Arc::new(taxonomy_id_array) as ArrayRef,
                Arc::new(division_id_array) as ArrayRef,
                Arc::new(sequence_array) as ArrayRef,
            ],
        )?;

        Ok(batch)
    }
}
