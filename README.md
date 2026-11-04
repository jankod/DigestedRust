# DigestedRust

Rust pipeline for building and searching a database of peptides produced by enzymatic digestion of UniProt proteins.

Project page: <https://digestedproteindb.pbf.hr/digestedrust.php>

## Download

Prebuilt binaries are attached to every [GitHub Release](../../releases/latest):

| Platform | Archive |
|----------|---------|
| Linux x86_64 | `digested-rust-<version>-linux-x86_64.tar.gz` |
| Linux aarch64 | `digested-rust-<version>-linux-aarch64.tar.gz` |
| macOS (Intel + Apple Silicon) | `digested-rust-<version>-macos-universal2.tar.gz` |
| Windows x86_64 | `digested-rust-<version>-windows-x86_64.zip` |

Each archive contains the `massq` search CLI and the database build tools `step1_…` – `step7_…`.
Python wheels for `massq_py` are attached to the same release (`pip install massq_py-*.whl`).
Checksums are in `SHA256SUMS.txt`.

```bash
tar -xzf digested-rust-<version>-linux-x86_64.tar.gz
./digested-rust-<version>-linux-x86_64/massq --help
```

---

## Python wrapper — `massq_py`

Python module that exposes the peptide database to web applications. The database stays open in memory (warm block cache), unlike the CLI tool, which opens and closes it on every invocation.

### Prerequisites

- Rust toolchain (stable) — [rustup.rs](https://rustup.rs)
- Python ≥ 3.9
- `uv` (recommended) or `pip`

### Installing maturin

```bash
uv tool install 'maturin[patchelf]'
```

### Build — editable install (for development)

```bash
cd DigestedRust
source python_orchestrator/.venv/bin/activate
maturin develop --features python
```

Maturin automatically creates a `.venv` virtualenv in the project directory and installs the module into it.
The first build takes ~2 minutes (it compiles RocksDB and all dependencies).
Subsequent builds are fast (incremental).

> **Note:** If you see a warning about `patchelf`, install it system-wide:
> `sudo apt install patchelf` (Ubuntu/Debian) or `sudo dnf install patchelf` (Fedora).
> The warning does not prevent the module from working.

### Build — release wheel (for production)

```bash
maturin build --release --features python
# the wheel is in: target/wheels/massq_py-*.whl
pip install target/wheels/massq_py-*.whl
```

### Usage in Python

```python
from massq_py import MassqDb

# Open the database (keep the object alive for the whole application lifetime)
db = MassqDb(
    db_path="/path/to/db/rocksdb",        # directory containing the CURRENT file
    block_cache_gb=8,                     # LRU cache in GB (recommended: 4–16)
    with_taxonomy=True,                   # True = load taxonomy.bin for species names
)

# Count peptides in a mass range [Da]
count = db.count_range(1000.0, 1001.0)
print(count)

# Search — JSON string
json_str = db.search_json(1000.0, 1001.0)
# '[{"mass": 1000.1234, "peptide": "PEPTIDE", "accession": "P12345", "tax_id": 9606, "species": "Homo sapiens"}, ...]'

# Search — CSV string (includes a header row)
csv_str = db.search_csv(1000.0, 1001.0)
# 'mass,peptide,accession,tax_id,species\n1000.1234,...'
```

#### `db_path` layout

The directory passed as `db_path` must contain:

```
rocksdb/
├── CURRENT          ← RocksDB marker
├── MANIFEST-*
├── *.sst
├── acc_dict.bin     ← accession ID → UniProt ID dictionary
└── taxonomy.bin     ← (optional) NCBI taxonomy for species lookup
```

If you use `with_taxonomy=True`, `taxonomy.bin` must be present.

#### Closing the database

There is no need to close the database explicitly. Rust `Drop` closes RocksDB automatically when the Python object is garbage collected (i.e. when Django shuts down). There is no `close()` method because it is never needed.

#### Using it in a Django application

Open the database **once** at application startup as a module-level singleton — do not open it in every request:

```python
# myapp/db.py  (imported once, lives for the whole process lifetime)
from massq_py import MassqDb

mass_db = MassqDb(
    db_path="/data/swisprot/rocksdb",
    block_cache_gb=8,
    with_taxonomy=True,
)
```

```python
# myapp/views.py
from django.http import JsonResponse
from .db import mass_db

def search(request):
    mass1 = float(request.GET["mass1"])
    mass2 = float(request.GET["mass2"])
    # The GIL is released automatically for the duration of the scan —
    # other Django threads can run in parallel.
    result = mass_db.search_json(mass1, mass2)
    return JsonResponse({"data": result}, safe=False)
```

#### Concurrent queries

All three methods (`search_json`, `search_csv`, `count_range`) **release the GIL** for the duration of the RocksDB scan via `py.allow_threads()`. This means:

- **Gunicorn with threads** (`--threads N`): queries run in parallel in Rust ✓
- **Gunicorn with workers** (`--workers N`): each worker has its own copy of the database in memory ✓
- **uvicorn / async**: be careful — the async event loop is single-threaded, but `allow_threads` still ensures queries do not block the event loop when called via `asyncio.to_thread()` ✓

RocksDB is thread-safe for concurrent reads, so no `Mutex` is needed.

#### Memory notes

`block_cache_gb` controls how much RAM RocksDB uses for the LRU cache of SST blocks.
Recommended values:
- Development: `2–4` GB
- Production (Swiss-Prot): `4–8` GB
- Production (TrEMBL): `16–32` GB

---

## CLI tool — `massq`

One-off search from the terminal (open → query → close).

```bash
cargo build --release --bin massq

./target/release/massq 1000.0 1001.0 --db-path /path/to/rocksdb
./target/release/massq 1000.0 1001.0 --db-path /path/to/rocksdb --with-taxonomy
./target/release/massq 1000.0 1001.0 --db-path /path/to/rocksdb --format csv
./target/release/massq 1000.0 1001.0 --db-path /path/to/rocksdb --count-only
```

---

## Database build pipeline

The steps run in order:

| Step | Binary | Description |
|------|--------|-------------|
| 1 | `step1_uniprot_xml_to_parquet` | Parse UniProt XML → Parquet |
| 2 | `step2_cleavage_protein` | Enzymatic protein cleavage |
| 3 | `step3_sort_parquet` | Sort by mass |
| 4 | `step4_populate_rocks_db` | Populate RocksDB |
| 5 | `step5_build_taxonomy_bin` | Build taxonomy index |
| 6 | `step6_build_dict_bin` | Build accession dictionary |
| 7 | `step7_validate_db` | Validate the database |
| 8 | `step8_package_db` | Package the database into archives (Python script, runs after manifest generation) |

### Building a database

The pipeline is orchestrated by `python_orchestrator/main.py`. It reads its configuration from a `db_builder.env` file in the build directory (`PATH_BUILD_DIR`), runs each step, and tracks progress in that directory.

1. **Create a build directory** where the database will be built, e.g. `databases_output/my_db/`.
2. **Configure it:** copy `examples/db_builder.env.example` into the build directory as `db_builder.env` and adjust it. That file documents every parameter; most are optional and can be omitted.
3. **Run** from the project root:

```bash
export PATH_BUILD_DIR=databases_output/my_db
python3 python_orchestrator/main.py
```

Additional options:

```bash
# Clear previous attempts in the build directory (keeps db_builder.env)
PATH_BUILD_DIR=databases_output/my_db python3 python_orchestrator/main.py --clear

# Run only selected steps (IDs 1–8), e.g. validation and packaging
PATH_BUILD_DIR=databases_output/my_db PIPELINE_STEP_IDS=7,8 python3 python_orchestrator/main.py

# Re-run steps that are already marked as completed
PATH_BUILD_DIR=databases_output/my_db PIPELINE_RERUN_COMPLETED=true python3 python_orchestrator/main.py
```

By default the orchestrator skips steps already completed successfully (as recorded in `logs/build_state.json`). Before starting, it runs `cargo build --release` for the required Rust binaries, so make sure Rust and all dependencies are installed.

Build directory layout:

```text
databases_output/<db_id>/
├── db_builder.env
├── manifest.toml            ← generated by the orchestrator at the end
├── logs/
│   ├── build_state.json     ← build state kept by the orchestrator
│   └── <step_bin>_result.json
├── intermediate_data/
└── final_db/rocksdb/        ← final database (PATH_ROCKSDB_OUTPUT_DIR)
```

Each step only reads its inputs (from environment variables), writes its artifacts, and writes a typed result JSON to `logs/<bin_name>_result.json`. Orchestration data (status, duration, step ID/name, start/end timestamps) is recorded by the orchestrator, not by the steps.

## Building massq / massq_py

```bash
PIPELINE_STEP_IDS=9 python3 python_orchestrator/main.py
```

## Benchmarking under a memory limit

```bash
systemd-run --user --scope -p MemoryMax=4G ./target/release/massq_bench \
  --db-path ./databases_output/my_db/final_db \
  --queries 100 \
  --ppm 10 \
  --mass-min 400 \
  --mass-max 4000 \
  --cold-runs 1 \
  --warm-runs 1 \
  --warmup-queries 10 \
  --read-fast-block-cache-gb 4 \
  --json-out ./databases_output/my_db/benchmarks/massq_bench.json
```

Alternatives:

```bash
ulimit -v $((4 * 1024 * 1024))   # ~4 GB, in KB

/usr/bin/time -v ./target/release/massq_bench ...
```

---

## License

Copyright (C) 2026 Janko Diminić

This program is free software: you can redistribute it and/or modify it under the terms of the
GNU Affero General Public License as published by the Free Software Foundation, either version 3
of the License, or (at your option) any later version. See [LICENSE](LICENSE).

If you run a modified version of this software as a network service, the AGPL requires you to make
the modified source code available to its users. For commercial licensing without these
obligations, see <https://digestedproteindb.pbf.hr/digestedrust.php>.
