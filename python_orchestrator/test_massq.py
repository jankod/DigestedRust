import os

from massq_py import MassqDb

# Open the database (keep the object alive for the whole lifetime of the application)
db = MassqDb(
    db_path=os.environ.get(
        "MASSQ_DB_PATH", "./databases_output/my_db/final_db/rocksdb"
    ),  # directory containing the CURRENT file
    block_cache_gb=4,                     # LRU cache in GB (recommended: 4–16)
    with_taxonomy=True,                   # True = load taxonomy.bin for species names
)

mass1 = 1200
mass2 = 1201
# Count peptides in the mass range [Da]
count = db.count_range(mass1, mass2)
print(count)

# Search — JSON string
#json_str = db.search_json(1000.0, 1001.0)
#print(json_str)

# Search — CSV string (includes a header row)
csv_str = db.search_csv(mass1, mass2)
print(csv_str)
