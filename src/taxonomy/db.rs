//! NCBI Taxonomy database — compact mmap-based lookup.
//!
//! Binary format (`taxonomy.bin`):
//! ```text
//! [max_tax_id: u32 LE][TaxNode × (max_tax_id + 1)][names blob UTF-8]
//! ```
//!
//! `TaxNode` is 11 bytes, `#[repr(C, packed)]`:
//! ```text
//! parent_id : u32   (LE)
//! rank      : u8
//! name_offset: u32  (LE, byte offset into names blob)
//! name_len  : u16   (LE)
//! ```

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use anyhow::Context;
use memmap2::Mmap;

// ── Rank enum ──

/// Taxonomic rank.  Stored as a single `u8` in the binary file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Rank {
    NoRank = 0,
    Superkingdom = 1,
    Kingdom = 2,
    Phylum = 3,
    Class = 4,
    Order = 5,
    Family = 6,
    Genus = 7,
    Species = 8,
    Subspecies = 9,
    Subphylum = 10,
    Subclass = 11,
    Suborder = 12,
    Subfamily = 13,
    Subgenus = 14,
    Superorder = 15,
    Superfamily = 16,
    Tribe = 17,
    Subtribe = 18,
    Infraorder = 19,
    Infraclass = 20,
    Parvorder = 21,
    Superphylum = 22,
    Superclass = 23,
    Cohort = 24,
    Forma = 25,
    Varietas = 26,
    Clade = 27,
    Other = 255,
}

impl Rank {
    pub fn from_str(s: &str) -> Self {
        match s.trim() {
            "no rank" => Rank::NoRank,
            "superkingdom" => Rank::Superkingdom,
            "kingdom" => Rank::Kingdom,
            "phylum" => Rank::Phylum,
            "class" => Rank::Class,
            "order" => Rank::Order,
            "family" => Rank::Family,
            "genus" => Rank::Genus,
            "species" => Rank::Species,
            "subspecies" => Rank::Subspecies,
            "subphylum" => Rank::Subphylum,
            "subclass" => Rank::Subclass,
            "suborder" => Rank::Suborder,
            "subfamily" => Rank::Subfamily,
            "subgenus" => Rank::Subgenus,
            "superorder" => Rank::Superorder,
            "superfamily" => Rank::Superfamily,
            "tribe" => Rank::Tribe,
            "subtribe" => Rank::Subtribe,
            "infraorder" => Rank::Infraorder,
            "infraclass" => Rank::Infraclass,
            "parvorder" => Rank::Parvorder,
            "superphylum" => Rank::Superphylum,
            "superclass" => Rank::Superclass,
            "cohort" => Rank::Cohort,
            "forma" => Rank::Forma,
            "varietas" => Rank::Varietas,
            "clade" => Rank::Clade,
            _ => Rank::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Rank::NoRank => "no rank",
            Rank::Superkingdom => "superkingdom",
            Rank::Kingdom => "kingdom",
            Rank::Phylum => "phylum",
            Rank::Class => "class",
            Rank::Order => "order",
            Rank::Family => "family",
            Rank::Genus => "genus",
            Rank::Species => "species",
            Rank::Subspecies => "subspecies",
            Rank::Subphylum => "subphylum",
            Rank::Subclass => "subclass",
            Rank::Suborder => "suborder",
            Rank::Subfamily => "subfamily",
            Rank::Subgenus => "subgenus",
            Rank::Superorder => "superorder",
            Rank::Superfamily => "superfamily",
            Rank::Tribe => "tribe",
            Rank::Subtribe => "subtribe",
            Rank::Infraorder => "infraorder",
            Rank::Infraclass => "infraclass",
            Rank::Parvorder => "parvorder",
            Rank::Superphylum => "superphylum",
            Rank::Superclass => "superclass",
            Rank::Cohort => "cohort",
            Rank::Forma => "forma",
            Rank::Varietas => "varietas",
            Rank::Clade => "clade",
            Rank::Other => "other",
        }
    }

    /// Returns true for ranks commonly shown in a condensed lineage.
    pub fn is_major(self) -> bool {
        matches!(
            self,
            Rank::Superkingdom
                | Rank::Kingdom
                | Rank::Phylum
                | Rank::Class
                | Rank::Order
                | Rank::Family
                | Rank::Genus
                | Rank::Species
        )
    }
}

// ── On-disk node (packed) ──

/// Packed on-disk representation. 11 bytes per node.
#[derive(Clone, Copy)]
#[repr(C, packed)]
struct TaxNodeRaw {
    parent_id: u32,
    rank: u8,
    name_offset: u32,
    name_len: u16,
}

const NODE_SIZE: usize = std::mem::size_of::<TaxNodeRaw>(); // 11

const HEADER_SIZE: usize = std::mem::size_of::<u32>(); // 4 bytes for max_tax_id

// ── Public entry returned by lookups ──

#[derive(Debug, Clone)]
pub struct TaxEntry<'a> {
    pub tax_id: u32,
    pub parent_id: u32,
    pub rank: Rank,
    pub name: &'a str,
}

// ── Mmap reader ──

pub struct TaxonomyDb {
    _mmap: Mmap,
    max_tax_id: u32,
    nodes_start: usize,
    names_start: usize,
}

impl TaxonomyDb {
    /// Memory-map an existing `taxonomy.bin` file.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let file = File::open(path).context(format!(
            "Failed to open taxonomy file '{}'",
            path.display()
        ))?;
        let mmap = unsafe { Mmap::map(&file)? };

        if mmap.len() < HEADER_SIZE {
            anyhow::bail!("Taxonomy file too small ({}B)", mmap.len());
        }

        let max_tax_id = u32::from_le_bytes(mmap[0..4].try_into().unwrap());
        let node_count = max_tax_id as usize + 1;
        let nodes_start = HEADER_SIZE;
        let names_start = nodes_start + node_count * NODE_SIZE;

        if mmap.len() < names_start {
            anyhow::bail!(
                "Taxonomy file truncated: need at least {} bytes for {} nodes, got {}",
                names_start,
                node_count,
                mmap.len()
            );
        }

        Ok(Self {
            _mmap: mmap,
            max_tax_id,
            nodes_start,
            names_start,
        })
    }

    /// Load taxonomy database with timing info printed to stderr.
    pub fn open_verbose(path: &Path) -> anyhow::Result<Self> {
        let start = Instant::now();
        eprintln!("Loading taxonomy from: {}", path.display());
        let db = Self::open(path)?;
        eprintln!(
            "Taxonomy loaded in {:?} ({} taxa, file {}B)",
            start.elapsed(),
            db.max_tax_id + 1,
            db._mmap.len()
        );
        Ok(db)
    }

    fn node_raw(&self, tax_id: u32) -> Option<TaxNodeRaw> {
        if tax_id > self.max_tax_id {
            return None;
        }
        let offset = self.nodes_start + (tax_id as usize) * NODE_SIZE;
        let end = offset + NODE_SIZE;
        if end > self._mmap.len() {
            return None;
        }
        let bytes = &self._mmap[offset..end];
        // Read packed fields manually (safe, no alignment issues)
        let parent_id = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let rank = bytes[4];
        let name_offset = u32::from_le_bytes(bytes[5..9].try_into().unwrap());
        let name_len = u16::from_le_bytes(bytes[9..11].try_into().unwrap());
        Some(TaxNodeRaw {
            parent_id,
            rank,
            name_offset,
            name_len,
        })
    }

    fn name_str(&self, node: &TaxNodeRaw) -> &str {
        if node.name_len == 0 {
            return "";
        }
        let start = self.names_start + node.name_offset as usize;
        let end = start + node.name_len as usize;
        if end > self._mmap.len() {
            return "";
        }
        std::str::from_utf8(&self._mmap[start..end]).unwrap_or("")
    }

    /// Look up a single tax_id.
    pub fn get(&self, tax_id: u32) -> Option<TaxEntry<'_>> {
        let node = self.node_raw(tax_id)?;
        // A node with parent_id == 0 and name_len == 0 is an empty slot (unused tax_id)
        if node.name_len == 0 && node.parent_id == 0 && tax_id != 1 {
            return None;
        }
        Some(TaxEntry {
            tax_id,
            parent_id: node.parent_id,
            rank: unsafe { std::mem::transmute::<u8, Rank>(node.rank) },
            name: self.name_str(&node),
        })
    }

    /// Get just the scientific name, or `None` if unknown.
    pub fn scientific_name(&self, tax_id: u32) -> Option<&str> {
        let node = self.node_raw(tax_id)?;
        if node.name_len == 0 && node.parent_id == 0 && tax_id != 1 {
            return None;
        }
        Some(self.name_str(&node))
    }

    /// Walk from `tax_id` up to the root, collecting entries.
    /// Returns entries from leaf towards root (i.e. species first, superkingdom last).
    pub fn lineage(&self, tax_id: u32) -> Vec<TaxEntry<'_>> {
        let mut result = Vec::with_capacity(16);
        let mut current = tax_id;
        // Guard against cycles (max depth 100)
        for _ in 0..100 {
            if let Some(entry) = self.get(current) {
                let parent = entry.parent_id;
                result.push(entry);
                if current == 1 || parent == current {
                    break;
                }
                current = parent;
            } else {
                break;
            }
        }
        result
    }

    /// Condensed lineage string showing only major ranks, root→leaf direction.
    /// Example: `"Bacteria > Proteobacteria > Gammaproteobacteria > Enterobacterales > Enterobacteriaceae > Escherichia > Escherichia coli"`
    pub fn lineage_string(&self, tax_id: u32) -> String {
        let entries = self.lineage(tax_id);
        let major: Vec<&str> = entries
            .iter()
            .rev()
            .filter(|e| e.rank.is_major())
            .map(|e| e.name)
            .collect();
        major.join(" > ")
    }

    pub fn max_tax_id(&self) -> u32 {
        self.max_tax_id
    }
}

// ── Builder — creates taxonomy.bin from NCBI dump files ──

pub struct TaxonomyDbBuilder {
    /// tax_id → (parent_id, rank)
    nodes: Vec<(u32, Rank)>,
    /// tax_id → scientific name
    names: HashMap<u32, String>,
    /// old_tax_id → new_tax_id
    merged: HashMap<u32, u32>,
    max_id: u32,
}

impl TaxonomyDbBuilder {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            names: HashMap::new(),
            merged: HashMap::new(),
            max_id: 0,
        }
    }

    /// Parse `nodes.dmp` — format: `tax_id\t|\tparent_tax_id\t|\trank\t|\t...`
    pub fn parse_nodes(&mut self, path: &Path) -> anyhow::Result<usize> {
        let file = File::open(path).context(format!("Failed to open {}", path.display()))?;
        let reader = BufReader::new(file);
        let mut count = 0usize;

        for line in reader.lines() {
            let line = line?;
            let parts: Vec<&str> = line.split("\t|\t").collect();
            if parts.len() < 3 {
                continue;
            }
            let tax_id: u32 = parts[0].trim().parse().context("bad tax_id")?;
            let parent_id: u32 = parts[1].trim().parse().context("bad parent_id")?;
            let rank = Rank::from_str(parts[2]);

            if tax_id > self.max_id {
                self.max_id = tax_id;
            }

            // Grow nodes vec to fit
            if tax_id as usize >= self.nodes.len() {
                self.nodes.resize((tax_id as usize + 1).max(self.nodes.len() * 2), (0, Rank::NoRank));
            }
            self.nodes[tax_id as usize] = (parent_id, rank);
            count += 1;
        }

        Ok(count)
    }

    /// Parse `names.dmp` — format: `tax_id\t|\tname_txt\t|\tunique_name\t|\tname_class\t|`
    /// We only keep `scientific name` entries.
    pub fn parse_names(&mut self, path: &Path) -> anyhow::Result<usize> {
        let file = File::open(path).context(format!("Failed to open {}", path.display()))?;
        let reader = BufReader::new(file);
        let mut count = 0usize;

        for line in reader.lines() {
            let line = line?;
            let parts: Vec<&str> = line.split("\t|\t").collect();
            if parts.len() < 4 {
                continue;
            }

            // name_class is the 4th field, but it may end with "\t|"
            let name_class = parts[3].trim().trim_end_matches('|').trim();
            if name_class != "scientific name" {
                continue;
            }

            let tax_id: u32 = match parts[0].trim().parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let name = parts[1].trim().to_string();

            self.names.insert(tax_id, name);
            count += 1;
        }

        Ok(count)
    }

    /// Parse `merged.dmp` — format: `old_tax_id\t|\tnew_tax_id\t|`
    pub fn parse_merged(&mut self, path: &Path) -> anyhow::Result<usize> {
        let file = File::open(path).context(format!("Failed to open {}", path.display()))?;
        let reader = BufReader::new(file);
        let mut count = 0usize;

        for line in reader.lines() {
            let line = line?;
            let parts: Vec<&str> = line.split("\t|\t").collect();
            if parts.len() < 2 {
                continue;
            }
            let old_id: u32 = match parts[0].trim().parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let new_id: u32 = match parts[1].trim().trim_end_matches('|').trim().parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            self.merged.insert(old_id, new_id);

            if old_id > self.max_id {
                self.max_id = old_id;
            }
            count += 1;
        }

        Ok(count)
    }

    /// Write the compact `taxonomy.bin` file.
    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        let file = File::create(path).context(format!("Failed to create {}", path.display()))?;
        let mut w = BufWriter::new(file);

        // Effective max_id including merged IDs
        let max_id = self.max_id;
        let node_count = max_id as usize + 1;

        // Header: max_tax_id (u32 LE)
        w.write_all(&max_id.to_le_bytes())?;

        // Build names blob and collect offsets
        let mut names_blob = Vec::<u8>::with_capacity(64 * 1024 * 1024);
        let mut name_offsets: HashMap<u32, (u32, u16)> = HashMap::with_capacity(self.names.len());

        for (&tax_id, name) in &self.names {
            let name_bytes = name.as_bytes();
            let offset = names_blob.len() as u32;
            let len = name_bytes.len().min(u16::MAX as usize) as u16;
            names_blob.extend_from_slice(&name_bytes[..len as usize]);
            name_offsets.insert(tax_id, (offset, len));
        }

        // Write node array
        let empty_node = [0u8; NODE_SIZE];
        for tax_id in 0..node_count as u32 {
            // Check if this is a merged ID — point to the target's data
            let effective_id = self.merged.get(&tax_id).copied().unwrap_or(tax_id);

            let has_node = (effective_id as usize) < self.nodes.len()
                && self.nodes[effective_id as usize] != (0, Rank::NoRank);

            if !has_node && !self.names.contains_key(&effective_id) {
                w.write_all(&empty_node)?;
                continue;
            }

            let (parent_id, rank) = if (effective_id as usize) < self.nodes.len() {
                self.nodes[effective_id as usize]
            } else {
                (0u32, Rank::NoRank)
            };

            let (name_offset, name_len) = name_offsets
                .get(&effective_id)
                .copied()
                .unwrap_or((0, 0));

            w.write_all(&parent_id.to_le_bytes())?;
            w.write_all(&[rank as u8])?;
            w.write_all(&name_offset.to_le_bytes())?;
            w.write_all(&name_len.to_le_bytes())?;
        }

        // Write names blob
        w.write_all(&names_blob)?;
        w.flush()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_test_nodes(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("nodes.dmp");
        let mut f = File::create(&path).unwrap();
        // root
        writeln!(f, "1\t|\t1\t|\tno rank\t|\t").unwrap();
        // Bacteria
        writeln!(f, "2\t|\t1\t|\tsuperkingdom\t|\t").unwrap();
        // Proteobacteria
        writeln!(f, "1224\t|\t2\t|\tphylum\t|\t").unwrap();
        // Gammaproteobacteria
        writeln!(f, "1236\t|\t1224\t|\tclass\t|\t").unwrap();
        // Enterobacterales
        writeln!(f, "91347\t|\t1236\t|\torder\t|\t").unwrap();
        // Enterobacteriaceae
        writeln!(f, "543\t|\t91347\t|\tfamily\t|\t").unwrap();
        // Escherichia
        writeln!(f, "561\t|\t543\t|\tgenus\t|\t").unwrap();
        // E. coli
        writeln!(f, "562\t|\t561\t|\tspecies\t|\t").unwrap();
        path
    }

    fn write_test_names(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("names.dmp");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "1\t|\troot\t|\t\t|\tscientific name\t|").unwrap();
        writeln!(f, "2\t|\tBacteria\t|\t\t|\tscientific name\t|").unwrap();
        writeln!(f, "2\t|\tMonera\t|\t\t|\tsynonym\t|").unwrap();
        writeln!(f, "1224\t|\tProteobacteria\t|\t\t|\tscientific name\t|").unwrap();
        writeln!(f, "1236\t|\tGammaproteobacteria\t|\t\t|\tscientific name\t|").unwrap();
        writeln!(f, "91347\t|\tEnterobacterales\t|\t\t|\tscientific name\t|").unwrap();
        writeln!(f, "543\t|\tEnterobacteriaceae\t|\t\t|\tscientific name\t|").unwrap();
        writeln!(f, "561\t|\tEscherichia\t|\t\t|\tscientific name\t|").unwrap();
        writeln!(f, "562\t|\tEscherichia coli\t|\t\t|\tscientific name\t|").unwrap();
        path
    }

    fn write_test_merged(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("merged.dmp");
        let mut f = File::create(&path).unwrap();
        // 999 was merged into 562 (E. coli)
        writeln!(f, "999\t|\t562\t|").unwrap();
        path
    }

    #[test]
    fn test_rank_from_str() {
        assert_eq!(Rank::from_str("species"), Rank::Species);
        assert_eq!(Rank::from_str("superkingdom"), Rank::Superkingdom);
        assert_eq!(Rank::from_str("no rank"), Rank::NoRank);
        assert_eq!(Rank::from_str("bogus"), Rank::Other);
    }

    #[test]
    fn test_rank_is_major() {
        assert!(Rank::Species.is_major());
        assert!(Rank::Phylum.is_major());
        assert!(!Rank::NoRank.is_major());
        assert!(!Rank::Tribe.is_major());
    }

    #[test]
    fn test_build_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let nodes_path = write_test_nodes(dir.path());
        let names_path = write_test_names(dir.path());
        let merged_path = write_test_merged(dir.path());
        let bin_path = dir.path().join("taxonomy.bin");

        // Build
        let mut builder = TaxonomyDbBuilder::new();
        let n_nodes = builder.parse_nodes(&nodes_path).unwrap();
        assert_eq!(n_nodes, 8);
        let n_names = builder.parse_names(&names_path).unwrap();
        assert_eq!(n_names, 8); // only scientific names
        let n_merged = builder.parse_merged(&merged_path).unwrap();
        assert_eq!(n_merged, 1);
        builder.write(&bin_path).unwrap();

        // Read
        let db = TaxonomyDb::open(&bin_path).unwrap();

        // Lookup E. coli
        let ecoli = db.get(562).unwrap();
        assert_eq!(ecoli.name, "Escherichia coli");
        assert_eq!(ecoli.rank, Rank::Species);
        assert_eq!(ecoli.parent_id, 561);

        // Lookup root
        let root = db.get(1).unwrap();
        assert_eq!(root.name, "root");

        // Lookup merged ID (999 → 562)
        let merged = db.get(999).unwrap();
        assert_eq!(merged.name, "Escherichia coli");

        // Non-existent ID
        assert!(db.get(88888).is_none());

        // scientific_name helper
        assert_eq!(db.scientific_name(2), Some("Bacteria"));
    }

    #[test]
    fn test_lineage_walk() {
        let dir = tempfile::tempdir().unwrap();
        let nodes_path = write_test_nodes(dir.path());
        let names_path = write_test_names(dir.path());
        let bin_path = dir.path().join("taxonomy.bin");

        let mut builder = TaxonomyDbBuilder::new();
        builder.parse_nodes(&nodes_path).unwrap();
        builder.parse_names(&names_path).unwrap();
        builder.write(&bin_path).unwrap();

        let db = TaxonomyDb::open(&bin_path).unwrap();

        // Lineage for E. coli (562): species → genus → family → order → class → phylum → superkingdom → root
        let lineage = db.lineage(562);
        let names: Vec<&str> = lineage.iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            vec![
                "Escherichia coli",
                "Escherichia",
                "Enterobacteriaceae",
                "Enterobacterales",
                "Gammaproteobacteria",
                "Proteobacteria",
                "Bacteria",
                "root",
            ]
        );

        // Condensed lineage string (root → leaf, major ranks only)
        let lin_str = db.lineage_string(562);
        assert_eq!(
            lin_str,
            "Bacteria > Proteobacteria > Gammaproteobacteria > Enterobacterales > Enterobacteriaceae > Escherichia > Escherichia coli"
        );
    }
}
