//! PyO3 extension module — exposes [`MassqDb`] to Python.
//!
//! Build with maturin:
//! ```sh
//! maturin develop --features python          # editable install
//! maturin build --release --features python  # wheel
//! ```
//!
//! Usage in Python:
//! ```python
//! from massq_py import MassqDb
//!
//! db = MassqDb("/data/swisprot/rocksdb", block_cache_gb=8, with_taxonomy=True)
//! print(db.count_range(1000.0, 1001.0))
//! results_json = db.search_json(1000.0, 1001.0)
//! results_csv  = db.search_csv(1000.0, 1001.0)
//! ```

use std::collections::HashMap;
use std::path::Path;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use rocksdb::{Direction, IteratorMode, ReadOptions, DB};
use serde::Serialize;

use crate::massq_core::{
    build_read_fast_db_options, load_accession_dict_mmap, MassGroup, MmapDict,
};
use crate::taxonomy::db::TaxonomyDb;
use crate::util::accession::from_accession_long36;
use crate::util::mass::{discretize_mass, undiscretize_mass};

// ── Internal result row ────────────────────────────────────────────────────────

#[derive(Serialize)]
struct PeptideRow {
    mass: f64,
    peptide: String,
    accession: String,
    tax_id: i32,
    /// Present only when the database was opened with `with_taxonomy = true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    species: Option<String>,
}

// ── Python class ───────────────────────────────────────────────────────────────

/// Python handle to an open DigestedRust mass database.
///
/// Keep the object alive for the lifetime of the web application so that the
/// RocksDB block cache and memory-mapped accession dictionary stay warm.
/// The object owns all open handles; no global state is used.
///
/// Thread safety: the underlying `rocksdb::DB` is `Send + Sync`; `MmapDict` is
/// marked `Send + Sync` in `massq_core` (read-only mmap, never mutated).
/// `TaxonomyDb` wraps the same kind of read-only mmap and is equally safe.
/// PyO3 acquires the GIL before calling any method, so concurrent Python calls
/// are serialised at the Python level without needing an additional `Mutex`.
#[pyclass]
pub struct MassqDb {
    db: DB,
    dict: MmapDict,
    taxonomy: Option<TaxonomyDb>,
}

// ── Private helpers (not exposed to Python) ────────────────────────────────────

impl MassqDb {
    /// Scan `[mass1, mass2]` and collect one `PeptideRow` per peptide record.
    ///
    /// Accession IDs and species names are cached per call to avoid repeated
    /// base-36 decoding and taxonomy lookups for the common case where many
    /// peptides share the same protein/organism.
    fn scan_range(&self, mass1: f64, mass2: f64) -> Result<Vec<PeptideRow>, String> {
        let m1 = discretize_mass(mass1);
        let m2 = discretize_mass(mass2);

        let mut read_options = ReadOptions::default();
        read_options.fill_cache(false);
        read_options.set_async_io(true);
        read_options.set_auto_readahead_size(true);
        // Exclusive upper bound lets RocksDB stop the iterator early.
        if m2 < u32::MAX {
            read_options.set_iterate_upper_bound((m2 + 1).to_be_bytes());
        }

        let start_key = m1.to_be_bytes();
        let iter = self.db.iterator_opt(
            IteratorMode::From(&start_key, Direction::Forward),
            read_options,
        );

        let mut rows: Vec<PeptideRow> = Vec::new();
        let mut acc_cache: HashMap<u32, String> = HashMap::with_capacity(1024);
        let mut species_cache: HashMap<u32, String> = HashMap::with_capacity(256);

        for item in iter {
            let (key, value) =
                item.map_err(|e| format!("RocksDB iterator error: {e}"))?;

            if key.len() != 4 {
                continue;
            }

            // key is always 4 bytes here — the check above ensures this.
            let key_arr: [u8; 4] = key.as_ref().try_into()
                .map_err(|_| "unexpected key length (not 4 bytes)".to_string())?;
            let current_mass = u32::from_be_bytes(key_arr);

            // Safety fallback in case the upper-bound hint is not respected.
            if current_mass > m2 {
                break;
            }

            let mass_f64 = undiscretize_mass(current_mass);

            match rkyv::api::high::from_bytes::<MassGroup, rkyv::rancor::Error>(&value[..]) {
                Ok(group) => {
                    for rec in group.records {
                        // Decode accession once per unique ID.
                        let accession = acc_cache
                            .entry(rec.accession_id)
                            .or_insert_with(|| {
                                self.dict
                                    .get(rec.accession_id)
                                    .map(from_accession_long36)
                                    .unwrap_or_else(|| format!("UNKNOWN_{}", rec.accession_id))
                            })
                            .clone();

                        // Resolve species name once per unique tax_id.
                        let species = match (self.taxonomy.as_ref(), u32::try_from(rec.tax_id)) {
                            (Some(tax_db), Ok(tax_u32)) => Some(
                                species_cache
                                    .entry(tax_u32)
                                    .or_insert_with(|| {
                                        tax_db
                                            .scientific_name(tax_u32)
                                            .unwrap_or("")
                                            .to_owned()
                                    })
                                    .clone(),
                            ),
                            _ => None,
                        };

                        rows.push(PeptideRow {
                            mass: mass_f64,
                            peptide: rec.peptide,
                            accession,
                            tax_id: rec.tax_id,
                            species,
                        });
                    }
                }
                Err(e) => {
                    // Log and skip corrupt records — do not abort the whole query.
                    eprintln!(
                        "[massq_py] Failed to deserialize record for mass {mass_f64:.4}: {e}"
                    );
                }
            }
        }

        Ok(rows)
    }
}

// ── Private helpers (count_range, no accession decoding) ──────────────────────

impl MassqDb {
    /// Fast count of peptide records in `[mass1, mass2]` — no GIL, no `PyErr`.
    ///
    /// Returns `Err(String)` so it can be called from inside `py.allow_threads()`
    /// (which requires the closure to be `Send + 'static`, meaning no `PyErr`
    /// because `PyErr` is `!Ungil`).
    fn count_range_inner(&self, mass1: f64, mass2: f64) -> Result<u64, String> {
        let m1 = discretize_mass(mass1);
        let m2 = discretize_mass(mass2);

        let mut read_options = ReadOptions::default();
        read_options.fill_cache(false);
        read_options.set_async_io(true);
        read_options.set_auto_readahead_size(true);
        if m2 < u32::MAX {
            read_options.set_iterate_upper_bound((m2 + 1).to_be_bytes());
        }

        let start_key = m1.to_be_bytes();
        let iter = self.db.iterator_opt(
            IteratorMode::From(&start_key, Direction::Forward),
            read_options,
        );

        let mut count: u64 = 0;
        for item in iter {
            let (key, value) =
                item.map_err(|e| format!("RocksDB iterator error: {e}"))?;

            if key.len() != 4 {
                continue;
            }

            let key_arr: [u8; 4] = key
                .as_ref()
                .try_into()
                .map_err(|_| "unexpected key length (not 4 bytes)".to_string())?;
            let current_mass = u32::from_be_bytes(key_arr);

            if current_mass > m2 {
                break;
            }

            match rkyv::api::high::from_bytes::<MassGroup, rkyv::rancor::Error>(&value[..]) {
                Ok(group) => count += group.records.len() as u64,
                Err(e) => eprintln!(
                    "[massq_py] Deserialise error at mass {:.4}: {e}",
                    undiscretize_mass(current_mass)
                ),
            }
        }

        Ok(count)
    }
}

// ── Public Python methods ──────────────────────────────────────────────────────

#[pymethods]
impl MassqDb {
    /// Open a DigestedRust mass database.
    ///
    /// Args:
    ///     db_path:        Path to the RocksDB directory (must contain a `CURRENT` file).
    ///                     Sibling files `acc_dict.bin` (and optionally `taxonomy.bin`)
    ///                     must also be present.
    ///     block_cache_gb: LRU block cache in GB.  Larger values improve repeated queries
    ///                     by keeping hot SST blocks in memory.  Must be >= 1.
    ///     with_taxonomy:  When True, also open `<db_path>/taxonomy.bin` to enable
    ///                     species-name resolution in search results.
    ///
    /// Raises:
    ///     RuntimeError: if any path cannot be opened or is invalid.
    #[new]
    pub fn initialize(
        db_path: &str,
        block_cache_gb: usize,
        with_taxonomy: bool,
    ) -> PyResult<Self> {
        if block_cache_gb == 0 {
            return Err(PyRuntimeError::new_err("block_cache_gb must be >= 1"));
        }

        let path = Path::new(db_path);

        let options = build_read_fast_db_options(block_cache_gb)
            .map_err(|e| PyRuntimeError::new_err(format!("DB options error: {e}")))?;

        let db = DB::open_for_read_only(&options, path, false).map_err(|e| {
            PyRuntimeError::new_err(format!("Failed to open RocksDB at '{db_path}': {e}"))
        })?;

        let dict_path = path.join("acc_dict.bin");
        let dict = load_accession_dict_mmap(&dict_path, false)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

        let taxonomy = if with_taxonomy {
            let tax_path = path.join("taxonomy.bin");
            Some(TaxonomyDb::open(&tax_path).map_err(|e| {
                PyRuntimeError::new_err(format!(
                    "Failed to open taxonomy at '{}': {e}",
                    tax_path.display()
                ))
            })?)
        } else {
            None
        };

        Ok(MassqDb { db, dict, taxonomy })
    }

    /// Count peptide records in the mass range `[mass1, mass2]` (inclusive, Da).
    ///
    /// Fast path: does not decode accessions or look up species names.
    /// Releases the GIL during the RocksDB scan so other Python threads can run.
    ///
    /// Returns:
    ///     Number of matching peptide records.
    ///
    /// Raises:
    ///     RuntimeError: on RocksDB iterator errors or deserialisation failures.
    pub fn count_range(&self, py: Python<'_>, mass1: f64, mass2: f64) -> PyResult<u64> {
        // `count_range_inner` returns `Result<u64, String>` — `String` is `Ungil`,
        // so the closure satisfies `py.detach`'s bounds (`allow_threads` was renamed
        // to `detach` in PyO3 0.24+).
        py.detach(|| self.count_range_inner(mass1, mass2))
            .map_err(PyRuntimeError::new_err)
    }

    /// Search for peptides in the mass range `[mass1, mass2]` (inclusive, Da).
    ///
    /// Releases the GIL during the RocksDB scan.
    ///
    /// Returns:
    ///     JSON string — an array of objects with fields:
    ///     `mass`, `peptide`, `accession`, `tax_id`, and optionally `species`
    ///     (present only when the database was opened with `with_taxonomy=True`).
    ///
    /// Raises:
    ///     RuntimeError: on iterator errors or JSON serialisation failure.
    pub fn search_json(&self, py: Python<'_>, mass1: f64, mass2: f64) -> PyResult<String> {
        // `scan_range` already returns `Result<_, String>`, so the closure is `Ungil`.
        // `py.detach` is the PyO3 0.24+ replacement for `allow_threads`.
        let rows = py
            .detach(|| self.scan_range(mass1, mass2))
            .map_err(PyRuntimeError::new_err)?;

        serde_json::to_string(&rows)
            .map_err(|e| PyRuntimeError::new_err(format!("JSON serialisation failed: {e}")))
    }

    /// Search for peptides in the mass range `[mass1, mass2]` (inclusive, Da).
    ///
    /// Releases the GIL during the RocksDB scan.
    ///
    /// Returns:
    ///     CSV string including a header row.
    ///     Columns: `mass,peptide,accession,tax_id` (plus `species` when taxonomy is loaded).
    ///
    /// Raises:
    ///     RuntimeError: on iterator errors.
    pub fn search_csv(&self, py: Python<'_>, mass1: f64, mass2: f64) -> PyResult<String> {
        // Scan without GIL via `py.detach` (PyO3 0.24+ replacement for `allow_threads`).
        let rows = py
            .detach(|| self.scan_range(mass1, mass2))
            .map_err(PyRuntimeError::new_err)?;

        let with_species = self.taxonomy.is_some();
        // Pre-allocate generously to avoid repeated reallocations.
        let mut out = String::with_capacity(rows.len() * 64 + 64);

        if with_species {
            out.push_str("mass,peptide,accession,tax_id,species\n");
        } else {
            out.push_str("mass,peptide,accession,tax_id\n");
        }

        for row in &rows {
            if with_species {
                out.push_str(&format!(
                    "{:.4},{},{},{},{}\n",
                    row.mass,
                    row.peptide,
                    row.accession,
                    row.tax_id,
                    row.species.as_deref().unwrap_or("")
                ));
            } else {
                out.push_str(&format!(
                    "{:.4},{},{},{}\n",
                    row.mass, row.peptide, row.accession, row.tax_id
                ));
            }
        }

        Ok(out)
    }
}

// ── Module entry point ─────────────────────────────────────────────────────────

/// Register the `massq_py` Python extension module.
///
/// Maturin picks this up automatically; the module name is set to `massq_py`
/// via `[tool.maturin] module-name` in `pyproject.toml`.
#[pymodule]
pub fn massq_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<MassqDb>()?;
    Ok(())
}
