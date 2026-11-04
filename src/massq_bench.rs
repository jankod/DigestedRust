use std::fs::{self, OpenOptions};
use std::io::{Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use serde::Serialize;
use sysinfo::{Disks, Pid, ProcessesToUpdate, System};

const DEFAULT_QUERIES: usize = 10_000;
const DEFAULT_PPM: f64 = 10.0;
const DEFAULT_MASS_MIN: f64 = 400.0;
const DEFAULT_MASS_MAX: f64 = 2000.0;
const DEFAULT_REAHEAD_MB: usize = 4;

#[derive(ValueEnum, Clone, Copy, Debug, Serialize, PartialEq, Eq)]
enum StdoutMode {
    Discard,
    Inherit,
    File,
}

#[derive(Parser, Debug)]
#[command(name = "massq_bench", version, about)]
struct Args {
    /// Path to massq binary (built already, no cargo)
    #[arg(long, value_name = "PATH", default_value = "target/release/massq")]
    massq_bin: PathBuf,

    /// Path to RocksDB dir or package root
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// Output JSON report path
    #[arg(long, value_name = "PATH", default_value = "benchmarks/massq_bench.json")]
    json_out: PathBuf,

    /// Total queries per run
    #[arg(long, default_value_t = DEFAULT_QUERIES)]
    queries: usize,

    /// Number of cold runs
    #[arg(long, default_value_t = 1)]
    cold_runs: usize,

    /// Number of warm runs
    #[arg(long, default_value_t = 1)]
    warm_runs: usize,

    /// Warmup queries executed before warm runs
    #[arg(long, default_value_t = 0)]
    warmup_queries: usize,

    /// Random seed for query generator
    #[arg(long, default_value_t = 42)]
    seed: u64,

    /// Mass range min
    #[arg(long, default_value_t = DEFAULT_MASS_MIN)]
    mass_min: f64,

    /// Mass range max
    #[arg(long, default_value_t = DEFAULT_MASS_MAX)]
    mass_max: f64,

    /// PPM window (e.g. 10.0)
    #[arg(long, default_value_t = DEFAULT_PPM)]
    ppm: f64,

    /// If true, massq runs with --count-only
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    massq_count_only: bool,

    /// What to do with massq stdout
    #[arg(long, value_enum, default_value_t = StdoutMode::Discard)]
    massq_stdout: StdoutMode,

    /// Path for massq stdout when mode=file
    #[arg(long, value_name = "PATH")]
    massq_stdout_path: Option<PathBuf>,

    /// Enable taxonomy resolution
    #[arg(long, action = clap::ArgAction::Set, default_value_t = false)]
    with_taxonomy: bool,

    /// Path to taxonomy.bin (implies taxonomy enabled)
    #[arg(long, value_name = "PATH")]
    taxonomy_path: Option<PathBuf>,

    /// RocksDB iterator readahead in MB
    #[arg(long, default_value_t = DEFAULT_REAHEAD_MB)]
    readahead_mb: usize,

    /// If true, fill RocksDB cache
    #[arg(long, action = clap::ArgAction::Set, default_value_t = false)]
    fill_cache: bool,

    /// Enable async IO in RocksDB
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    async_io: bool,

    /// Enable auto readahead in RocksDB
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    auto_readahead: bool,

    /// Override MASSQ_READ_FAST_BLOCK_CACHE_GB
    #[arg(long)]
    read_fast_block_cache_gb: Option<usize>,
}

#[derive(Clone, Debug)]
struct Query {
    mass1: f64,
    mass2: f64,
}

#[derive(Serialize)]
struct BenchmarkReport {
    generated_at: String,
    config: BenchmarkConfig,
    system: SystemInfo,
    database: DatabaseInfo,
    runs: Vec<RunSummary>,
}

#[derive(Serialize)]
struct BenchmarkConfig {
    massq_bin: String,
    db_path: String,
    rocksdb_path: String,
    queries: usize,
    cold_runs: usize,
    warm_runs: usize,
    warmup_queries: usize,
    seed: u64,
    mass_min: f64,
    mass_max: f64,
    ppm: f64,
    massq_count_only: bool,
    massq_stdout: StdoutMode,
    readahead_mb: usize,
    fill_cache: bool,
    async_io: bool,
    auto_readahead: bool,
    read_fast_block_cache_gb: Option<usize>,
    with_taxonomy: bool,
    taxonomy_path: Option<String>,
}

#[derive(Serialize)]
struct SystemInfo {
    os: String,
    cpu_model: String,
    cpu_cores: usize,
    total_memory_bytes: u64,
    total_memory_gb: f64,
    disk: Option<DiskInfo>,
}

#[derive(Serialize)]
struct DiskInfo {
    name: String,
    mount_point: String,
    file_system: String,
    total_bytes: u64,
    available_bytes: u64,
}

#[derive(Serialize)]
struct DatabaseInfo {
    input_db_path: String,
    rocksdb_path: String,
    rocksdb_size_bytes: u64,
    manifest_path: Option<String>,
    manifest: Option<serde_json::Value>,
    massq_version: Option<String>,
}

#[derive(Serialize)]
struct RunSummary {
    run_type: String,
    queries: usize,
    errors: usize,
    first_latency_ms: Option<f64>,
    total_time_ms: f64,
    peak_rss_bytes: u64,
    latency_ms: LatencyStats,
}

#[derive(Serialize, Default)]
struct LatencyStats {
    min_ms: f64,
    max_ms: f64,
    mean_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if !args.massq_bin.is_file() {
        anyhow::bail!(
            "massq binary not found: {}\n\nBuild it or set --massq-bin. Example:\n  cargo build --release --bin massq\n  ./target/release/massq_bench --massq-bin ./target/release/massq ...",
            args.massq_bin.display()
        );
    }

    if args.queries == 0 {
        anyhow::bail!("queries must be >= 1");
    }
    if args.mass_min >= args.mass_max {
        anyhow::bail!("mass_min must be < mass_max");
    }
    if args.ppm <= 0.0 {
        anyhow::bail!("ppm must be > 0");
    }
    if args.massq_stdout == StdoutMode::File && args.massq_stdout_path.is_none() {
        anyhow::bail!("massq_stdout_path must be set when massq_stdout=file");
    }

    let rocksdb_path = resolve_rocksdb_dir(&args.db_path);
    let queries = generate_queries(
        args.queries,
        args.seed,
        args.mass_min,
        args.mass_max,
        args.ppm,
    );

    let system_info = collect_system_info(&rocksdb_path);
    let manifest_path = find_manifest_path(&args.db_path, &rocksdb_path);
    let manifest = manifest_path
        .as_ref()
        .and_then(|p| load_manifest(p).ok());
    let massq_version = get_massq_version(&args.massq_bin).ok();

    let db_info = DatabaseInfo {
        input_db_path: args.db_path.to_string_lossy().to_string(),
        rocksdb_path: rocksdb_path.to_string_lossy().to_string(),
        rocksdb_size_bytes: dir_size_bytes(&rocksdb_path)
            .context("Failed to measure RocksDB size")?,
        manifest_path: manifest_path.as_ref().map(|p| p.to_string_lossy().to_string()),
        manifest,
        massq_version,
    };

    let config = BenchmarkConfig {
        massq_bin: args.massq_bin.to_string_lossy().to_string(),
        db_path: args.db_path.to_string_lossy().to_string(),
        rocksdb_path: rocksdb_path.to_string_lossy().to_string(),
        queries: args.queries,
        cold_runs: args.cold_runs,
        warm_runs: args.warm_runs,
        warmup_queries: args.warmup_queries,
        seed: args.seed,
        mass_min: args.mass_min,
        mass_max: args.mass_max,
        ppm: args.ppm,
        massq_count_only: args.massq_count_only,
        massq_stdout: args.massq_stdout,
        readahead_mb: args.readahead_mb,
        fill_cache: args.fill_cache,
        async_io: args.async_io,
        auto_readahead: args.auto_readahead,
        read_fast_block_cache_gb: args.read_fast_block_cache_gb,
        with_taxonomy: args.with_taxonomy,
        taxonomy_path: args.taxonomy_path.as_ref().map(|p| p.to_string_lossy().to_string()),
    };

    let mut runs = Vec::new();

    for idx in 0..args.cold_runs {
        let label = format!("cold_{idx}");
        let summary = run_batch(&label, &args, &queries, 0)?;
        runs.push(summary);
    }

    for idx in 0..args.warm_runs {
        let label = format!("warm_{idx}");
        let summary = run_batch(&label, &args, &queries, args.warmup_queries)?;
        runs.push(summary);
    }

    let report = BenchmarkReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        config,
        system: system_info,
        database: db_info,
        runs,
    };

    if let Some(parent) = args.json_out.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("Failed to create output dir: {}", parent.display())
        })?;
    }
    let json = serde_json::to_string_pretty(&report)?;
    fs::write(&args.json_out, json)?;

    println!("Report written to {}", args.json_out.display());
    Ok(())
}

fn run_batch(
    label: &str,
    args: &Args,
    queries: &[Query],
    warmup: usize,
) -> Result<RunSummary> {
    if warmup > 0 {
        for q in queries.iter().take(warmup) {
            run_massq_query(args, q)?;
        }
    }

    let mut latencies = Vec::with_capacity(queries.len());
    let mut errors = 0usize;
    let mut peak_rss_bytes = 0u64;
    let mut first_latency_ms = None;
    let start = Instant::now();

    for (idx, q) in queries.iter().enumerate() {
        let result = run_massq_query(args, q);
        match result {
            Ok(measurement) => {
                if idx == 0 {
                    first_latency_ms = Some(measurement.latency_ms);
                }
                latencies.push(measurement.latency_ms);
                if measurement.peak_rss_bytes > peak_rss_bytes {
                    peak_rss_bytes = measurement.peak_rss_bytes;
                }
            }
            Err(err) => {
                errors += 1;
                eprintln!("[{label}] query failed: {err:#}");
            }
        }
    }

    let total_time_ms = start.elapsed().as_secs_f64() * 1000.0;
    let latency_stats = compute_latency_stats(&mut latencies);

    Ok(RunSummary {
        run_type: label.to_string(),
        queries: queries.len(),
        errors,
        first_latency_ms,
        total_time_ms,
        peak_rss_bytes,
        latency_ms: latency_stats,
    })
}

struct QueryMeasurement {
    latency_ms: f64,
    peak_rss_bytes: u64,
}

fn run_massq_query(args: &Args, query: &Query) -> Result<QueryMeasurement> {
    let mut cmd = Command::new(&args.massq_bin);
    cmd.arg(format!("{:.6}", query.mass1))
        .arg(format!("{:.6}", query.mass2))
        .arg("--db-path")
        .arg(&args.db_path)
        .arg("--readahead-mb")
        .arg(args.readahead_mb.to_string())
        .arg("--async-io")
        .arg(format!("{}", args.async_io))
        .arg("--auto-readahead")
        .arg(format!("{}", args.auto_readahead));

    if args.fill_cache {
        cmd.arg("--fill-cache");
    }

    if args.massq_count_only {
        cmd.arg("--count-only");
    } else {
        cmd.arg("--format").arg("csv");
    }

    if let Some(tax) = &args.taxonomy_path {
        cmd.arg("--taxonomy").arg(tax);
    } else if args.with_taxonomy {
        cmd.arg("--with-taxonomy");
    }

    if let Some(cache_gb) = args.read_fast_block_cache_gb {
        cmd.env("MASSQ_READ_FAST_BLOCK_CACHE_GB", cache_gb.to_string());
    }

    let stdout_mode = args.massq_stdout;
    let stdout_handle = match stdout_mode {
        StdoutMode::Discard => Stdio::null(),
        StdoutMode::Inherit => Stdio::inherit(),
        StdoutMode::File => {
            let path = args
                .massq_stdout_path
                .as_ref()
                .context("massq_stdout_path missing")?;
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("Failed to open stdout path: {}", path.display()))?;
            Stdio::from(file)
        }
    };

    let mut child = cmd
        .stdout(stdout_handle)
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to run massq: {}", args.massq_bin.display()))?;

    let start = Instant::now();
    let pid = Pid::from_u32(child.id());
    let mut sys = System::new();
    let mut peak_rss_kb = 0u64;

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), false);
        if let Some(process) = sys.process(pid) {
            let mem_kb = process.memory();
            if mem_kb > peak_rss_kb {
                peak_rss_kb = mem_kb;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr)?;
    }

    if !status.success() {
        if stderr.is_empty() {
            anyhow::bail!("massq exited with code: {status}");
        } else {
            anyhow::bail!("massq failed: {stderr}");
        }
    }

    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
    let peak_rss_bytes = peak_rss_kb.saturating_mul(1024);

    Ok(QueryMeasurement {
        latency_ms,
        peak_rss_bytes,
    })
}

fn compute_latency_stats(latencies: &mut [f64]) -> LatencyStats {
    if latencies.is_empty() {
        return LatencyStats::default();
    }

    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let min_ms = latencies[0];
    let max_ms = latencies[latencies.len() - 1];
    let mean_ms = latencies.iter().sum::<f64>() / latencies.len() as f64;

    LatencyStats {
        min_ms,
        max_ms,
        mean_ms,
        p50_ms: percentile(latencies, 0.50),
        p95_ms: percentile(latencies, 0.95),
        p99_ms: percentile(latencies, 0.99),
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }

    let n = sorted.len() as f64;
    let rank = (n - 1.0) * p;
    let low = rank.floor() as usize;
    let high = rank.ceil() as usize;
    if low == high {
        sorted[low]
    } else {
        let w = rank - low as f64;
        sorted[low] + (sorted[high] - sorted[low]) * w
    }
}

fn generate_queries(count: usize, seed: u64, min_mass: f64, max_mass: f64, ppm: f64) -> Vec<Query> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut out = Vec::with_capacity(count);

    for _ in 0..count {
        let center = rng.random_range(min_mass..max_mass);
        let delta = center * ppm / 1_000_000.0;
        let mass1 = (center - delta).max(0.0);
        let mass2 = center + delta;
        out.push(Query { mass1, mass2 });
    }

    out
}

fn resolve_rocksdb_dir(input: &Path) -> PathBuf {
    let candidate = input.join("rocksdb");
    if candidate.is_dir() {
        candidate
    } else {
        input.to_path_buf()
    }
}

fn find_manifest_path(db_path: &Path, rocksdb_path: &Path) -> Option<PathBuf> {
    let mut candidates = vec![
        db_path.join("manifest.toml"),
        rocksdb_path.join("manifest.toml"),
    ];
    if let Some(parent) = rocksdb_path.parent() {
        candidates.push(parent.join("manifest.toml"));
    }

    candidates.into_iter().find(|p| p.is_file())
}

fn load_manifest(path: &Path) -> Result<serde_json::Value> {
    let text = fs::read_to_string(path)?;
    let toml_value: toml::Value = toml::from_str(&text)?;
    let json_value = serde_json::to_value(toml_value)?;
    Ok(json_value)
}

fn get_massq_version(path: &Path) -> Result<String> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .with_context(|| format!("Failed to run massq --version: {}", path.display()))?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        Ok(String::from("unknown"))
    } else {
        Ok(text)
    }
}

fn collect_system_info(db_path: &Path) -> SystemInfo {
    let mut sys = System::new_all();
    sys.refresh_all();

    let os = System::long_os_version()
        .or_else(System::name)
        .unwrap_or_else(|| "unknown".to_string());
    let cpu_model = detect_cpu_model(&sys);
    let cpu_cores = sys.cpus().len();
    let total_memory_bytes = sys.total_memory().saturating_mul(1024);
    let total_memory_gb = total_memory_bytes as f64 / 1_073_741_824.0;

    let disks = Disks::new_with_refreshed_list();
    let disk = select_disk(&disks, db_path).map(|d| DiskInfo {
        name: d.name().to_string_lossy().to_string(),
        mount_point: d.mount_point().to_string_lossy().to_string(),
        file_system: d.file_system().to_string_lossy().to_string(),
        total_bytes: d.total_space(),
        available_bytes: d.available_space(),
    });

    SystemInfo {
        os,
        cpu_model,
        cpu_cores,
        total_memory_bytes,
        total_memory_gb,
        disk,
    }
}

fn detect_cpu_model(sys: &System) -> String {
    let model = sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default();
    if !model.is_empty() {
        return model;
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = fs::read_to_string("/proc/cpuinfo") {
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("model name") {
                    if let Some(value) = rest.split(':').nth(1) {
                        let trimmed = value.trim();
                        if !trimmed.is_empty() {
                            return trimmed.to_string();
                        }
                    }
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
        {
            let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !value.is_empty() {
                return value;
            }
        }
    }

    "unknown".to_string()
}

fn select_disk<'a>(disks: &'a Disks, path: &Path) -> Option<&'a sysinfo::Disk> {
    let mut best: Option<&sysinfo::Disk> = None;
    let mut best_len = 0usize;

    for disk in disks.iter() {
        let mount = disk.mount_point();
        if path.starts_with(mount) {
            let len = mount.as_os_str().len();
            if len >= best_len {
                best = Some(disk);
                best_len = len;
            }
        }
    }

    best
}

fn dir_size_bytes(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            total = total.saturating_add(dir_size_bytes(&entry.path())?);
        } else {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

