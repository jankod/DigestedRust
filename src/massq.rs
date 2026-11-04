use std::collections::HashMap;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context;
use clap::{ArgAction, Parser, ValueEnum};
use rocksdb::{Direction, IteratorMode, ReadOptions, DB};
use semver::Version;
use toml::Value as TomlValue;

use digested_rust::massq_core::{
    build_read_fast_db_options, load_accession_dict_mmap, MassGroup, MmapDict,
    READ_FAST_BLOCK_CACHE_GB, READ_FAST_MAX_OPEN_FILES,
};
use digested_rust::taxonomy::db::TaxonomyDb;
use digested_rust::util::accession::from_accession_long36;
use digested_rust::util::mass::{discretize_mass, undiscretize_mass};

/// Default sequential prefetch size used by RocksDB iterators (in megabytes).
/// Larger values can improve long forward range scans, while smaller values
/// can reduce memory pressure for short/random queries.
const DEFAULT_READAHEAD_MB: usize = 4;
/// Output buffering for stdout to reduce syscall overhead during large prints.
const STDOUT_BUFFER_SIZE: usize = 1 << 20;
/// Schema version supported by this massq binary.
const SUPPORTED_DB_SCHEMA_VERSION: &str = "1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Default,
    Csv,
    Tree,
}

/// Unified mass query tool for RocksDB, with optional taxonomy resolution.
#[derive(Parser, Debug)]
#[command(name = "massq", version, about)]
struct Cli {
    /// Start mass (lower bound)
    mass1: f64,

    /// End mass (upper bound)
    mass2: f64,

    /// Path to RocksDB directory OR package parent directory.
    /// If this path is not a direct RocksDB dir, massq tries `<path>/rocksdb`.
    #[arg(long, value_name = "PATH")]
    db_path: Option<PathBuf>,

    /// Enable taxonomy lookups using default path (<db_path>/taxonomy.bin)
    #[arg(long)]
    with_taxonomy: bool,

    /// Path to taxonomy.bin (also enables taxonomy lookups)
    #[arg(long, value_name = "PATH")]
    taxonomy: Option<PathBuf>,

    /// Print full taxonomy lineage (enables taxonomy lookups)
    #[arg(long)]
    lineage: bool,

    /// Output format
    #[arg(long, value_enum, default_value_t = OutputFormat::Default)]
    format: OutputFormat,

    /// Fast mode: do not print records; print only total count in range
    #[arg(long)]
    count_only: bool,

    /// RocksDB iterator prefetch size in MB.
    /// Set to 0 to disable fixed prefetch and rely on RocksDB defaults.
    #[arg(long, default_value_t = DEFAULT_READAHEAD_MB)]
    readahead_mb: usize,

    /// READ_FAST profile block cache size in GB.
    #[arg(long, env = "MASSQ_READ_FAST_BLOCK_CACHE_GB", default_value_t = READ_FAST_BLOCK_CACHE_GB)]
    read_fast_block_cache_gb: usize,

    /// If enabled, scanned blocks are inserted into RocksDB block cache.
    /// Keep disabled for one-off bulk scans to avoid cache pollution.
    #[arg(long)]
    fill_cache: bool,

    /// Enable RocksDB async I/O for prefetch during forward iteration.
    #[arg(long, action = ArgAction::Set, default_value_t = true)]
    async_io: bool,

    /// Let RocksDB adapt prefetch size automatically during long scans.
    #[arg(long, action = ArgAction::Set, default_value_t = true)]
    auto_readahead: bool,

    /// Verbose output
    #[arg(long, short)]
    verbose: bool,
}

struct Config {
    mass1: f64,
    mass2: f64,
    db_path: PathBuf,
    taxonomy_path: Option<PathBuf>,
    show_lineage: bool,
    output_format: OutputFormat,
    count_only: bool,
    readahead_bytes: usize,
    fill_cache: bool,
    async_io: bool,
    auto_readahead: bool,
    read_fast_block_cache_gb: usize,
    verbose: bool,
}

fn resolve_db_path(
    cli_value: Option<&Path>,
    default_paths: &[&str],
    verbose: bool,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = cli_value {
        if verbose {
            eprintln!(
                "[config] RocksDB database from CLI argument: {}",
                path.display()
            );
        }
        return Ok(path.to_path_buf());
    }

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    for default in default_paths {
        let cwd_path = Path::new(default);
        if cwd_path.exists() {
            if verbose {
                eprintln!(
                    "[config] RocksDB database found (CWD): {}",
                    cwd_path.display()
                );
            }
            return Ok(cwd_path.to_path_buf());
        }

        if let Some(ref exe) = exe_dir {
            let exe_path = exe.join(default);
            if exe_path.exists() {
                if verbose {
                    eprintln!(
                        "[config] RocksDB database found next to executable: {}",
                        exe_path.display()
                    );
                }
                return Ok(exe_path);
            }
        }
    }

    anyhow::bail!(
        "Cannot find RocksDB database. Specify --db-path or place it at one of: {:?}",
        default_paths
    )
}

fn is_likely_rocksdb_dir(path: &Path) -> bool {
    path.is_dir() && path.join("CURRENT").is_file()
}

fn resolve_runtime_rocksdb_path(input_path: PathBuf, verbose: bool) -> anyhow::Result<PathBuf> {
    if is_likely_rocksdb_dir(&input_path) {
        return Ok(input_path);
    }

    // Package layout fallback: if user passes parent folder created by step8 extraction,
    // try its nested rocksdb directory automatically.
    let fallback = input_path.join("rocksdb");
    if is_likely_rocksdb_dir(&fallback) {
        if verbose {
            eprintln!(
                "[config] --db-path points to package parent; using nested RocksDB dir: {}",
                fallback.display()
            );
        }
        return Ok(fallback);
    }

    anyhow::bail!(
        "RocksDB directory not found at '{}' and fallback '{}' is not a valid RocksDB directory.",
        input_path.display(),
        fallback.display()
    )
}

fn build_config(cli: Cli) -> anyhow::Result<Config> {
    if cli.mass1 > cli.mass2 {
        anyhow::bail!("mass1 ({}) must be <= mass2 ({})", cli.mass1, cli.mass2);
    }
    if cli.read_fast_block_cache_gb == 0 {
        anyhow::bail!("read_fast_block_cache_gb must be >= 1");
    }

    let db_path_candidate = resolve_db_path(
        cli.db_path.as_deref(),
        &["output/db", "data/db", "db"],
        cli.verbose,
    )?;
    let db_path = resolve_runtime_rocksdb_path(db_path_candidate, cli.verbose)?;

    let taxonomy_enabled = cli.with_taxonomy || cli.taxonomy.is_some() || cli.lineage;
    let taxonomy_path = if taxonomy_enabled {
        Some(cli.taxonomy.unwrap_or_else(|| db_path.join("taxonomy.bin")))
    } else {
        None
    };

    let readahead_bytes = cli
        .readahead_mb
        .checked_mul(1024 * 1024)
        .context("readahead_mb is too large")?;

    Ok(Config {
        mass1: cli.mass1,
        mass2: cli.mass2,
        db_path,
        taxonomy_path,
        show_lineage: cli.lineage,
        output_format: cli.format,
        count_only: cli.count_only,
        readahead_bytes,
        fill_cache: cli.fill_cache,
        async_io: cli.async_io,
        auto_readahead: cli.auto_readahead,
        read_fast_block_cache_gb: cli.read_fast_block_cache_gb,
        verbose: cli.verbose,
    })
}

fn validate_manifest_compatibility(db_path: &Path, verbose: bool) -> anyhow::Result<()> {
    let manifest_path = db_path
        .parent()
        .map(|p| p.join("manifest.toml"))
        .unwrap_or_else(|| PathBuf::from("manifest.toml"));

    if !manifest_path.exists() {
        if verbose {
            eprintln!(
                "[compat] manifest.toml not found next to DB, skipping compatibility check."
            );
        }
        return Ok(());
    }

    let manifest_raw = std::fs::read_to_string(&manifest_path).with_context(|| {
        format!(
            "Failed to read manifest file for compatibility check: {}",
            manifest_path.display()
        )
    })?;

    let manifest_value: TomlValue = manifest_raw.parse::<TomlValue>().with_context(|| {
        format!(
            "Failed to parse manifest TOML for compatibility check: {}",
            manifest_path.display()
        )
    })?;

    let Some(package_table) = manifest_value.get("package").and_then(|v| v.as_table()) else {
        if verbose {
            eprintln!(
                "[compat] No [package] section in {}, skipping compatibility check.",
                manifest_path.display()
            );
        }
        return Ok(());
    };

    if let Some(schema_version) = package_table
        .get("db_schema_version")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        if schema_version != SUPPORTED_DB_SCHEMA_VERSION {
            anyhow::bail!(
                "Incompatible database schema version '{}'. This massq supports schema '{}'.",
                schema_version,
                SUPPORTED_DB_SCHEMA_VERSION
            );
        }
    }

    if let Some(min_massq_version) = package_table
        .get("min_massq_version")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        let required = Version::parse(min_massq_version).with_context(|| {
            format!(
                "Invalid manifest package.min_massq_version '{}' in {}",
                min_massq_version,
                manifest_path.display()
            )
        })?;
        let current = Version::parse(env!("CARGO_PKG_VERSION"))
            .context("Failed to parse current massq version")?;

        if current < required {
            anyhow::bail!(
                "massq version {} is too old for this database. Required: >= {}.",
                current,
                required
            );
        }
    }

    if verbose {
        eprintln!(
            "[compat] Database package compatibility check passed: {}",
            manifest_path.display()
        );
    }
    Ok(())
}

fn accession_cached<'a>(id: u32, dict: &MmapDict, cache: &'a mut HashMap<u32, String>) -> &'a str {
    use std::collections::hash_map::Entry;

    // Reuse decoded accessions because many peptides share the same accession_id.
    match cache.entry(id) {
        Entry::Occupied(entry) => entry.into_mut().as_str(),
        Entry::Vacant(entry) => {
            let value = dict
                .get(id)
                .map(from_accession_long36)
                .unwrap_or_else(|| format!("UNKNOWN_{id}"));
            entry.insert(value).as_str()
        }
    }
}

fn species_cached<'a>(
    tax_id: i32,
    taxonomy: &TaxonomyDb,
    cache: &'a mut HashMap<u32, String>,
) -> &'a str {
    use std::collections::hash_map::Entry;

    let Ok(tax_u32) = u32::try_from(tax_id) else {
        return "";
    };

    // Cache tax_id -> species name to avoid repeated taxonomy lookups.
    match cache.entry(tax_u32) {
        Entry::Occupied(entry) => entry.into_mut().as_str(),
        Entry::Vacant(entry) => {
            let value = taxonomy.scientific_name(tax_u32).unwrap_or("").to_owned();
            entry.insert(value).as_str()
        }
    }
}

fn lineage_cached<'a>(
    tax_id: i32,
    taxonomy: &TaxonomyDb,
    cache: &'a mut HashMap<u32, String>,
) -> &'a str {
    use std::collections::hash_map::Entry;

    let Ok(tax_u32) = u32::try_from(tax_id) else {
        return "";
    };

    // Cache tax_id -> lineage because lineage construction is relatively expensive.
    match cache.entry(tax_u32) {
        Entry::Occupied(entry) => entry.into_mut().as_str(),
        Entry::Vacant(entry) => {
            let value = taxonomy.lineage_string(tax_u32);
            entry.insert(value).as_str()
        }
    }
}

fn main() -> anyhow::Result<()> {
    let config = build_config(Cli::parse())?;
    validate_manifest_compatibility(&config.db_path, config.verbose)?;

    let mass1 = discretize_mass(config.mass1);
    let mass2 = discretize_mass(config.mass2);

    // In count-only mode we skip loading any auxiliary data and only count rows.
    let dict = if config.count_only {
        None
    } else {
        let dict_path = config.db_path.join("acc_dict.bin");
        Some(load_accession_dict_mmap(&dict_path, config.verbose)?)
    };

    let taxonomy = if config.count_only {
        None
    } else if let Some(path) = config.taxonomy_path.as_ref() {
        if !path.exists() {
            anyhow::bail!(
                "Taxonomy file not found: {}\nBuild it with: cargo run --release --bin step5_build_taxonomy_bin -- --nodes <nodes.dmp> --names <names.dmp> --merged <merged.dmp> --output {}\nOr pass --taxonomy <path>.",
                path.display(),
                path.display()
            );
        }

        if config.verbose {
            Some(TaxonomyDb::open_verbose(path)?)
        } else {
            Some(TaxonomyDb::open(path)?)
        }
    } else {
        None
    };

    if config.verbose {
        eprintln!("Opening RocksDB at: {}", config.db_path.display());
    }
    let options = build_read_fast_db_options(config.read_fast_block_cache_gb)?;
    let db = DB::open_for_read_only(&options, &config.db_path, false)?;

    // ReadOptions are tuned for forward range scans.
    let mut read_options = ReadOptions::default();
    read_options.fill_cache(config.fill_cache);
    read_options.set_async_io(config.async_io);
    read_options.set_auto_readahead_size(config.auto_readahead);
    if config.readahead_bytes > 0 {
        read_options.set_readahead_size(config.readahead_bytes);
    }
    // Exclusive upper bound allows RocksDB to stop the iterator earlier.
    if mass2 < u32::MAX {
        read_options.set_iterate_upper_bound((mass2 + 1).to_be_bytes());
    }

    if config.verbose {
        eprintln!(
            "Scanning mass range [{:.4}, {:.4}] (readahead={}MB, fill_cache={}, async_io={}, auto_readahead={})...",
            config.mass1,
            config.mass2,
            config.readahead_bytes / (1024 * 1024),
            config.fill_cache,
            config.async_io,
            config.auto_readahead,
        );
        eprintln!(
            "READ_FAST profile: block_cache={}GB, max_open_files={}",
            config.read_fast_block_cache_gb, READ_FAST_MAX_OPEN_FILES
        );
    }

    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(STDOUT_BUFFER_SIZE, stdout.lock());

    if !config.count_only {
        match config.output_format {
            OutputFormat::Csv => match (taxonomy.is_some(), config.show_lineage) {
                (false, _) => writeln!(out, "mass,peptide,accession,tax_id")?,
                (true, false) => writeln!(out, "mass,peptide,accession,tax_id,species")?,
                (true, true) => writeln!(out, "mass,peptide,accession,tax_id,species,lineage")?,
            },
            OutputFormat::Default => match (taxonomy.is_some(), config.show_lineage) {
                (false, _) => writeln!(out, "#\tMass\tPeptide\tAccession\tTaxID")?,
                (true, false) => writeln!(out, "#\tMass\tPeptide\tAccession\tTaxID\tSpecies")?,
                (true, true) => {
                    writeln!(out, "#\tMass\tPeptide\tAccession\tTaxID\tSpecies\tLineage")?
                }
            },
            OutputFormat::Tree => {}
        }
    }

    let start_key = mass1.to_be_bytes();
    let iter = db.iterator_opt(
        IteratorMode::From(&start_key, Direction::Forward),
        read_options,
    );

    let scan_start = Instant::now();
    let mut count: u64 = 0;

    let mut accession_cache = HashMap::<u32, String>::with_capacity(4096);
    let mut species_cache = HashMap::<u32, String>::with_capacity(4096);
    let mut lineage_cache = HashMap::<u32, String>::with_capacity(4096);

    let mut last_mass: Option<u32> = None;
    let mut last_peptide = String::new();
    let mut mass_index: u64 = 0;

    for item in iter {
        let (key, value) = item?;
        if key.len() != 4 {
            continue;
        }

        let current_mass = u32::from_be_bytes(key.as_ref().try_into().unwrap());

        // Safety fallback in case upper bound is not respected for any reason.
        if current_mass > mass2 {
            break;
        }

        let mass_f64 = undiscretize_mass(current_mass);

        match rkyv::api::high::from_bytes::<MassGroup, rkyv::rancor::Error>(&value[..]) {
            Ok(group) => {
                if config.count_only {
                    count += group.records.len() as u64;
                    continue;
                }

                if matches!(
                    config.output_format,
                    OutputFormat::Tree | OutputFormat::Default
                ) && last_mass != Some(current_mass)
                {
                    mass_index += 1;
                    match config.output_format {
                        OutputFormat::Tree => writeln!(out, "M\t{}\t{:.4}", mass_index, mass_f64)?,
                        OutputFormat::Default => writeln!(out, "{}\t{:.4}", mass_index, mass_f64)?,
                        OutputFormat::Csv => {}
                    }
                    last_mass = Some(current_mass);
                    last_peptide.clear();
                }

                for rec in group.records {
                    let accession = accession_cached(
                        rec.accession_id,
                        dict.as_ref()
                            .expect("dict present when count_only is false"),
                        &mut accession_cache,
                    );

                    if let Some(tax_db) = taxonomy.as_ref() {
                        let species = species_cached(rec.tax_id, tax_db, &mut species_cache);

                        if config.show_lineage {
                            let lineage = lineage_cached(rec.tax_id, tax_db, &mut lineage_cache);

                            match config.output_format {
                                OutputFormat::Tree => {
                                    if rec.peptide != last_peptide {
                                        writeln!(out, "\tP\t{}", rec.peptide)?;
                                        last_peptide = rec.peptide.clone();
                                    }
                                    if lineage.is_empty() {
                                        writeln!(
                                            out,
                                            "\t\tA\t{}\t{}\t{}",
                                            accession, rec.tax_id, species
                                        )?;
                                    } else {
                                        writeln!(
                                            out,
                                            "\t\tA\t{}\t{}\t{}\t[{}]",
                                            accession, rec.tax_id, species, lineage
                                        )?;
                                    }
                                }
                                OutputFormat::Csv => {
                                    writeln!(
                                        out,
                                        "{:.4},{},{},{},{},{}",
                                        mass_f64,
                                        rec.peptide,
                                        accession,
                                        rec.tax_id,
                                        species,
                                        lineage
                                    )?;
                                }
                                OutputFormat::Default => {
                                    writeln!(
                                        out,
                                        "\t\t{}\t{}\t{}\t{}\t{}",
                                        rec.peptide, accession, rec.tax_id, species, lineage
                                    )?;
                                }
                            }
                        } else {
                            match config.output_format {
                                OutputFormat::Tree => {
                                    if rec.peptide != last_peptide {
                                        writeln!(out, "\tP\t{}", rec.peptide)?;
                                        last_peptide = rec.peptide.clone();
                                    }
                                    writeln!(
                                        out,
                                        "\t\tA\t{}\t{}\t{}",
                                        accession, rec.tax_id, species
                                    )?;
                                }
                                OutputFormat::Csv => {
                                    writeln!(
                                        out,
                                        "{:.4},{},{},{},{}",
                                        mass_f64, rec.peptide, accession, rec.tax_id, species
                                    )?;
                                }
                                OutputFormat::Default => {
                                    writeln!(
                                        out,
                                        "\t\t{}\t{}\t{}\t{}",
                                        rec.peptide, accession, rec.tax_id, species
                                    )?;
                                }
                            }
                        }
                    } else {
                        match config.output_format {
                            OutputFormat::Tree => {
                                if rec.peptide != last_peptide {
                                    writeln!(out, "\tP\t{}", rec.peptide)?;
                                    last_peptide = rec.peptide.clone();
                                }
                                writeln!(out, "\t\tA\t{}\t{}", accession, rec.tax_id)?;
                            }
                            OutputFormat::Csv => {
                                writeln!(
                                    out,
                                    "{:.4},{},{},{}",
                                    mass_f64, rec.peptide, accession, rec.tax_id
                                )?;
                            }
                            OutputFormat::Default => {
                                writeln!(
                                    out,
                                    "\t\t{}\t{}\t{}",
                                    rec.peptide, accession, rec.tax_id
                                )?;
                            }
                        }
                    }

                    count += 1;
                }
            }
            Err(error) => {
                eprintln!(
                    "Failed to deserialize record for mass {:.4}: {}",
                    mass_f64, error
                );
            }
        }
    }

    if config.count_only {
        writeln!(out, "{}", count)?;
    }

    out.flush()?;

    if config.verbose {
        eprintln!("Found {} peptides in {:?}", count, scan_start.elapsed());
    }

    Ok(())
}
