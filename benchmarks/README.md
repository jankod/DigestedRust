# massq benchmark

A small runner that invokes the `massq` binary and writes a JSON summary.

## Prerequisites
- `massq` binary built at `target/release/massq` (or set `--massq-bin`).

## Build
```bash
cargo build --release --bin massq
cargo build --release --bin massq_bench
```

## Running (example)
```bash
./target/release/massq_bench \
  --db-path /path/to/db_or_package \
  --queries 10000 \
  --ppm 10 \
  --mass-min 400 \
  --mass-max 2000 \
  --cold-runs 1 \
  --warm-runs 1 \
  --warmup-queries 1000 \
  --json-out benchmarks/massq_bench.json
```

## Output
- JSON summary with metrics and metadata (OS/CPU/RAM/disk + manifest.toml if present).
- If you want CSV from `massq`, run without `--massq-count-only` and set:
  - `--massq-stdout file --massq-stdout-path benchmarks/massq_output.csv`

Note: the CSV will contain a header for every query (massq repeats the header).

## More examples

```bash
./target/release/massq_bench \
  --db-path /path/to/trembl_all_deep/rocksdb \
  --queries 10000 \
  --ppm 10 \
  --mass-min 400 \
  --mass-max 2000 \
  --cold-runs 1 \
  --warm-runs 1 \
  --warmup-queries 1000 \
  --json-out /path/to/trembl_all_deep/benchmarks/massq_bench.json
```

```bash
./target/release/massq_bench \
  --db-path ./databases_output/my_db/final_db \
  --queries 100 \
  --ppm 10 \
  --mass-min 400 \
  --mass-max 4000 \
  --cold-runs 1 \
  --warm-runs 1 \
  --warmup-queries 10 \
  --massq-count-only false \
  --read-fast-block-cache-gb 4 \
  --json-out ./databases_output/my_db/benchmarks/massq_bench.json
```
