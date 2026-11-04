# Benchmark plan for massq

## Goal
A standardized benchmark that can be compared across different databases and machines (Linux/Windows/macOS).
The benchmark invokes an already built `massq` binary (from `target/`), measures query latencies and writes a JSON summary.

## Scenarios
- **Cold run**: no warmup queries.
- **Warm run**: run `warmup_queries` queries before measuring.

## Queries
- `queries` queries in total per run.
- Each query is a mass window `center +/- ppm`.
- `center` is drawn uniformly from the range `mass_min..mass_max`.
- The seed is deterministic for comparability.

## Metrics
Required:
- median (p50) latency
- p95 latency
- p99 latency
- cold start time (first query in a cold run)
- peak RAM (peak RSS of the `massq` process)
- index size (size of the RocksDB directory)

## Metadata (for comparison)
- **Database**: manifest.toml (if present), RocksDB size, path
- **Hardware**: OS, CPU model + core count, RAM, disk (mount, FS, total/available)

## Output
- JSON summary with metrics and metadata
- Optional: CSV produced by `massq` during the queries (unless `--count-only`)

## JSON output (fields)
Example file names: `benchmarks/massq_bench.json` or `benchmarks/massq_bench2.json`.

- `generated_at`: ISO timestamp of when the report was created.
- `config`: benchmark input settings.
  - `massq_bin`: path to the `massq` binary.
  - `db_path`: path passed to the benchmark.
  - `rocksdb_path`: actual path used (if `db_path` is a package, then `db_path/rocksdb`).
  - `queries`: number of queries per run.
  - `cold_runs`: number of cold runs (each run has no warmup and measures the first query as the cold start).
  - `warm_runs`: number of warm runs (`warmup_queries` are run before each run).
  - `warmup_queries`: number of warmup queries before a warm run.
  - `seed`: seed for the query generator.
  - `mass_min`, `mass_max`: mass range used to generate queries.
  - `ppm`: ppm window.
  - `massq_count_only`: if true, `massq` does not print CSV.
  - `massq_stdout`: what happens to stdout (`discard`, `inherit`, `file`).
  - `readahead_mb`: RocksDB iterator prefetch in MB per query (0 = RocksDB default).
  - `fill_cache`, `async_io`, `auto_readahead`: RocksDB options.
  - `read_fast_block_cache_gb`: cache override (env var).
  - `with_taxonomy`: enable taxonomy lookup.
  - `taxonomy_path`: explicit path to `taxonomy.bin`.
- `system`: hardware and OS information.
  - `os`: OS name (sysinfo).
  - `cpu_model`: CPU model (`unknown` if not available).
  - `cpu_cores`: number of cores.
  - `total_memory_bytes`: total RAM in bytes.
  - `total_memory_gb`: total RAM in GB.
  - `disk`: disk the database resides on.
    - `name`, `mount_point`, `file_system`, `total_bytes`, `available_bytes`.
- `database`: database metadata.
  - `input_db_path`: original input path.
  - `rocksdb_path`: actual RocksDB path.
  - `rocksdb_size_bytes`: size of the RocksDB directory.
  - `manifest_path`: path to `manifest.toml`, if present.
  - `manifest`: parsed `manifest.toml` as JSON.
  - `massq_version`: output of `massq --version`.
- `runs`: list of runs.
  - `run_type`: `cold_0`, `warm_0`, etc. (run type and index; `cold_*` has no warmup, `warm_*` has warmup).
  - `queries`: number of queries executed in that run (should match `config.queries`).
  - `errors`: number of queries that failed (excluded from latency statistics).
  - `first_latency_ms`: latency of the first query in the run; for `cold_*` this is the cold start, for `warm_*` the first measured query after warmup.
  - `total_time_ms`: total run time from the start of the first measured query to the end of the last one (includes both successful and failed queries).
  - `peak_rss_bytes`: highest measured RSS of the `massq` process during the run (an estimate, sampled at short intervals).
  - `latency_ms`: latency statistics over successful queries.
    - `min_ms`: shortest latency in the run.
    - `max_ms`: longest latency in the run.
    - `mean_ms`: arithmetic mean of latencies.
    - `p50_ms`: median (50th percentile).
    - `p95_ms`: 95th percentile.
    - `p99_ms`: 99th percentile.
