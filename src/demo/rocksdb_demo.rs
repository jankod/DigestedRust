use rocksdb::{DB, Options};
use std::error::Error;

/// Converts an f32 into bytes that sort correctly in RocksDB.
/// Since masses are always positive, big-endian encoding works correctly
/// for lexicographic ordering.
fn mass_to_key(mass: f32) -> [u8; 4] {
    mass.to_be_bytes()
}

/// Converts bytes back into an f32 mass.
fn key_to_mass(bytes: &[u8]) -> f32 {
    let mut arr = [0u8; 4];
    arr.copy_from_slice(bytes);
    f32::from_be_bytes(arr)
}

pub struct MassDatabase {
    db: DB,
}

impl MassDatabase {
    /// Opens or creates a RocksDB database at the given path.
    pub fn open(path: &str) -> Result<Self, Box<dyn Error>> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        // Optional: optimization for sorted writes when filling the database in one go
        // opts.prepare_for_bulk_load(); 
        
        let db = DB::open(&opts, path)?;
        Ok(MassDatabase { db })
    }

    /// Inserts a mass and its associated text into the database.
    pub fn insert(&self, mass: f32, text: &str) -> Result<(), Box<dyn Error>> {
        let key = mass_to_key(mass);
        self.db.put(key, text.as_bytes())?;
        Ok(())
    }

    /// Reads the text for an exact mass.
    pub fn get(&self, mass: f32) -> Result<Option<String>, Box<dyn Error>> {
        let key = mass_to_key(mass);
        let result = self.db.get(key)?;
        match result {
            Some(bytes) => Ok(Some(String::from_utf8(bytes)?)),
            None => Ok(None),
        }
    }

    /// Reads all records within a mass range (e.g. 500.0 to 1000.0).
    /// RocksDB returns data sorted by key (mass).
    pub fn read_range(&self, start_mass: f32, end_mass: f32) -> Result<Vec<(f32, String)>, Box<dyn Error>> {
        let start_key = mass_to_key(start_mass);
        let end_key = mass_to_key(end_mass);
        
        let mut results = Vec::new();
        let iter = self.db.iterator(rocksdb::IteratorMode::From(&start_key, rocksdb::Direction::Forward));
        
        for item in iter {
            let (key, value) = item?;
            if key.as_ref() > &end_key {
                break;
            }
            let mass = key_to_mass(&key);
            let text = String::from_utf8(value.to_vec())?;
            results.push((mass, text));
        }
        
        Ok(results)
    }

    /// Prints all records in the database (demonstrates ordering).
    pub fn print_all(&self) -> Result<(), Box<dyn Error>> {
        let iter = self.db.iterator(rocksdb::IteratorMode::Start);
        for item in iter {
            let (key, value) = item?;
            let mass = key_to_mass(&key);
            let text = String::from_utf8(value.to_vec())?;
            println!("Mass: {:.4}, Data: {}", mass, text);
        }
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let path = "demo_rocksdb_storage";
    
    // 1. Open the database
    let db = MassDatabase::open(path)?;
    
    // 2. Fill the database with unsorted data (RocksDB sorts it by itself)
    println!("Populating database...");
    db.insert(1234.5678, "Protein A fragment")?;
    db.insert(500.20, "Peptide X")?;
    db.insert(2000.10, "Large molecule")?;
    db.insert(750.0, "Medium fragment")?;
    db.insert(100.5, "Small fragment")?;
    
    // 3. Read all data (printed sorted by mass)
    println!("
All records in the database (automatically sorted):");
    db.print_all()?;
    
    // 4. Mass range search (e.g. 600 to 1500)
    println!("
Searching masses in range 600.0 - 1500.0:");
    let range = db.read_range(600.0, 1500.0)?;
    for (m, t) in range {
        println!(" - Found: {:.4} -> {}", m, t);
    }
    
    // 5. Read a specific mass
    if let Some(txt) = db.get(500.20)? {
        println!("
Found for 500.20: {}", txt);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_test_db(path: &str) -> MassDatabase {
        let _ = fs::remove_dir_all(path); // Remove previous test data
        MassDatabase::open(path).expect("Failed to open test database")
    }

    #[test]
    fn test_insert_and_get() {
        let path = "test_rocksdb_storage_1";
        let db = setup_test_db(path);
        
        db.insert(100.5, "Test Protein").unwrap();
        
        let result = db.get(100.5).unwrap();
        assert_eq!(result, Some("Test Protein".to_string()));
        
        let not_found = db.get(200.0).unwrap();
        assert_eq!(not_found, None);
        
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn test_range_scan_sorting() {
        let path = "test_rocksdb_storage_2";
        let db = setup_test_db(path);
        
        // Insert in deliberately random order
        db.insert(500.0, "Medium").unwrap();
        db.insert(100.0, "Small").unwrap();
        db.insert(1000.0, "Large").unwrap();
        
        let range = db.read_range(50.0, 1500.0).unwrap();
        
        // Verify results are returned sorted by mass
        assert_eq!(range.len(), 3);
        assert_eq!(range[0].0, 100.0);
        assert_eq!(range[1].0, 500.0);
        assert_eq!(range[2].0, 1000.0);
        
        let partial_range = db.read_range(400.0, 600.0).unwrap();
        assert_eq!(partial_range.len(), 1);
        assert_eq!(partial_range[0].1, "Medium");
        
        let _ = fs::remove_dir_all(path);
    }
}
