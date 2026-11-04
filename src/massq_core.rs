//! Core types and database helpers shared between the `massq` CLI binary
//! and the `massq_py` Python extension module.
//!
//! Nothing in this module is Python-specific — it is plain Rust.

use std::fs::File;
use std::path::Path;
use std::time::Instant;

use anyhow::Context;
use memmap2::Mmap;
use rkyv::{Archive, Deserialize, Serialize};
use rocksdb::{BlockBasedOptions, Cache, Options};

// ── Constants ──────────────────────────────────────────────────────────────────

/// RocksDB option: keep all table files open (recommended on Linux servers).
pub const READ_FAST_MAX_OPEN_FILES: i32 = -1;

/// Default block cache size for the READ_FAST profile (in GB).
pub const READ_FAST_BLOCK_CACHE_GB: usize = 8;

// ── Serialised types ───────────────────────────────────────────────────────────

/// A single peptide record stored under a discretised mass key.
#[derive(Archive, Deserialize, Serialize, Debug, PartialEq)]
#[rkyv(compare(PartialEq))]
#[rkyv(derive(Debug))]
pub struct PeptideRecord {
    pub peptide: String,
    pub accession_id: u32,
    pub tax_id: i32,
}

/// A group of peptide records sharing the same discretised mass key.
#[derive(Archive, Deserialize, Serialize, Debug, PartialEq, Default)]
#[rkyv(compare(PartialEq))]
#[rkyv(derive(Debug))]
pub struct MassGroup {
    pub records: Vec<PeptideRecord>,
}

// ── Accession dictionary (mmap) ────────────────────────────────────────────────

/// Memory-mapped accession dictionary: maps `accession_id: u32` → packed base-36 `u64`.
///
/// The file is opened read-only and never written after construction.
pub struct MmapDict {
    pub(crate) _mmap: Mmap,
    pub(crate) len_u64: usize,
}

// SAFETY: MmapDict wraps a read-only memory-mapped file that is never mutated after
// construction.  All accesses go through shared immutable references to the mapped
// slice, so sending across threads and sharing concurrently is safe.
unsafe impl Send for MmapDict {}
unsafe impl Sync for MmapDict {}

impl MmapDict {
    #[inline]
    fn slice(&self) -> &[u64] {
        // SAFETY: lifetime is tied to self, len_u64 and pointer alignment are
        // validated during construction in `load_accession_dict_mmap`.
        unsafe { std::slice::from_raw_parts(self._mmap.as_ptr() as *const u64, self.len_u64) }
    }

    /// Return the packed base-36 `u64` for `id`, or `None` for unused slots / out-of-range.
    pub fn get(&self, id: u32) -> Option<u64> {
        let idx = id as usize;
        if idx < self.len_u64 {
            let value = self.slice()[idx];
            if value != 0 {
                return Some(value);
            }
        }
        None
    }
}

// ── Loader ─────────────────────────────────────────────────────────────────────

/// Memory-map and validate the accession dictionary binary (`acc_dict.bin`).
pub fn load_accession_dict_mmap(dict_path: &Path, verbose: bool) -> anyhow::Result<MmapDict> {
    if verbose {
        eprintln!("Loading accession dictionary from: {}", dict_path.display());
    }
    let start = Instant::now();

    let file = File::open(dict_path).context(format!(
        "Failed to open dictionary file '{}'.\n\
         Expected 'acc_dict.bin' in the RocksDB database directory.",
        dict_path.display()
    ))?;

    let mmap = unsafe { Mmap::map(&file)? };

    let elem_size = std::mem::size_of::<u64>();
    if mmap.len() % elem_size != 0 {
        anyhow::bail!(
            "Dictionary file size {} is not divisible by {} (u64)",
            mmap.len(),
            elem_size
        );
    }
    if (mmap.as_ptr() as usize) % std::mem::align_of::<u64>() != 0 {
        anyhow::bail!("Mapped dictionary data is not u64-aligned");
    }

    let len_u64 = mmap.len() / elem_size;

    if verbose {
        eprintln!("Loaded accession dictionary in {:?}", start.elapsed());
    }

    Ok(MmapDict { _mmap: mmap, len_u64 })
}

// ── RocksDB options ────────────────────────────────────────────────────────────

/// Build `Options` tuned for fast repeated reads with a warm LRU block cache.
///
/// Bloom filters are intentionally omitted: the database is accessed via sequential
/// range scans (not point lookups), so filters would waste memory without benefit.
///
/// # Errors
/// Returns an error if `block_cache_gb` overflows `usize` when converted to bytes.
pub fn build_read_fast_db_options(block_cache_gb: usize) -> anyhow::Result<Options> {
    let block_cache_bytes = block_cache_gb
        .checked_mul(1024 * 1024 * 1024)
        .context("read_fast_block_cache_gb is too large")?;

    let mut options = Options::default();
    options.create_if_missing(false);
    options.set_allow_mmap_reads(false);
    options.set_max_open_files(READ_FAST_MAX_OPEN_FILES);

    let mut table = BlockBasedOptions::default();
    let cache = Cache::new_lru_cache(block_cache_bytes);
    table.set_block_cache(&cache);
    table.set_cache_index_and_filter_blocks(true);
    table.set_pin_l0_filter_and_index_blocks_in_cache(true);
    options.set_block_based_table_factory(&table);

    Ok(options)
}
