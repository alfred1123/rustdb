# catalog/

System catalog module — manages the database's internal metadata.

## Files

| File           | Purpose                                                    |
|----------------|------------------------------------------------------------|
| `types.rs`     | Struct definitions: `Tablespace`, `Schema`, `Table`, `Column`, `BufferPool`, `Catalog` |
| `row.rs`       | `RowReader` / `RowWriter` — binary serialization using u64 LE length-prefixed fields |
| `config.rs`    | `DbConfig` — reads/writes the `admin/SQLDBCONF` database configuration file |
| `bootstrap.rs` | Creates a fresh database directory with `SQLDBCONF` and system catalog `.DAT` files |
| `loader.rs`    | Reads `.DAT` files from disk and deserializes them into `Catalog` |
| `cache.rs`     | `CatalogCache` — permanent in-memory cache with O(1) HashMap lookups for tables, columns, tablespaces |

## How It Works

1. **Bootstrap** (`bootstrap.rs`) writes `admin/SQLDBCONF` and the five system tables (`SYSTABLESPACES`, `SYSSCHEMAS`, `SYSTABLES`, `SYSCOLUMNS`, `SYSBUFFERPOOLS`) into `systbsp/` as `.DAT` files.
2. **Config** (`config.rs`) reads `SQLDBCONF` on subsequent startups so the engine knows the database's settings.
3. **Loader** (`loader.rs`) reads those `.DAT` files back, splitting them into rows and deserializing each row with `RowReader`.
4. **Cache** (`cache.rs`) wraps the loaded `Catalog` in a `CatalogCache` that stays resident for the lifetime of the database.  It pre-materializes all catalog rows as `Vec<Value>` and builds `HashMap` indexes for O(1) lookup by name or ID.  The SQL executor reads exclusively from this cache — no per-query struct conversion or linear scans.
5. The row format is simple: each field is `[u64 LE length][value bytes]`. Types like `SMALLINT` and `INTEGER` are stored as little-endian fixed-width integers; strings are raw UTF-8 bytes; booleans are `Y`/`N` flag bytes.

## Text Mode (`--text-mode`)

When the `--text-mode` flag is passed, both bootstrap and loader use **tab-separated (TSV)** format instead of binary. The `.DAT` files become human-readable with a header row, making it easy to inspect catalog data with `cat` or any text editor.

Both modes share the same per-table functions — the `text_mode` flag controls branching internally so data definitions are never duplicated.

## Database Configuration (`admin/SQLDBCONF`)

Written at bootstrap and read on every subsequent startup. Format: `KEY = VALUE` with `--` comments.

| Parameter   | Default | Description                                               |
|-------------|---------|-----------------------------------------------------------|
| `PAGESIZE`  | `4096`  | Default page size (bytes) for new tablespaces. Each tablespace stores its own `PAGESIZE` in `SYSTABLESPACES`. Must be a power of two ≥ 512. |
| `DIAGLEVEL` | `INFO`  | Diagnostic verbosity: `OFF`, `ERROR`, `WARN`, `INFO`, `DEBUG`, `TRACE`. |
| `TEXT_MODE`  | `FALSE` | `TRUE` = TSV text `.DAT` files, `FALSE` = binary.         |

## Data Representations

### Tablespace TBSPACETYPE (DB2: `SYSCAT.TABLESPACES.TBSPACETYPE`)

Specifies how the tablespace storage is managed.

| Code | Meaning                                                      |
|------|--------------------------------------------------------------|
| `D`  | **Database-managed space (DMS)** — RustDB manages files directly |
| `S`  | **System-managed space (SMS)** — OS manages files (not used)  |

### Tablespace DATATYPE (DB2: `SYSCAT.TABLESPACES.DATATYPE`)

Specifies what kind of data can be stored in the tablespace.

| Code | Meaning                                                              |
|------|----------------------------------------------------------------------|
| `A`  | **All types** — regular permanent data (e.g., `SYSTBSP`, `USERTBSP`) |
| `L`  | **Large** — LOB (large object) storage (planned)                      |
| `T`  | **System temporary** — transient data for sorts, joins, etc. (e.g., `TEMPTBSP`) |
| `U`  | **User temporary** — user-created temporary tables (planned)          |

### Tablespace STATE

| Code | Meaning        |
|------|----------------|
| `N`  | **Normal**     |

### Column NULLABLE

| Code | Meaning        |
|------|----------------|
| `Y`  | Nullable       |
| `N`  | Not nullable   |

### SYSTABLESPACES.BUFFERPOOLID

Each tablespace references a buffer pool by ID. The default mapping is:

| Tablespace | BUFFERPOOLID | Pool Name |
|------------|-------------|-----------|
| SYSTBSP    | 1           | RQDEFAULTBP |
| USERTBSP   | 1           | RQDEFAULTBP |
| TEMPTBSP   | 4           | TEMPBP |

### SYSBUFFERPOOLS

Defines available buffer pools. Columns: `BPID`, `BPNAME`, `PAGESIZE`, `NPAGES`.

| BPID | BPNAME       | PAGESIZE | NPAGES | Purpose |
|------|--------------|----------|--------|---------|
| 1    | RQDEFAULTBP  | 4096     | 128    | Default data pool |
| 2    | INDEXBP      | 4096     | 64     | Index pages |
| 3    | LOBBP        | 32768    | 32     | Large objects |
| 4    | TEMPBP       | 4096     | 64     | Temporary/sort |

### SQL Type Mappings

| TYPE_NAME      | Rust storage      | Binary size |
|----------------|-------------------|-------------|
| `SMALLINT`     | `i16`             | 2 bytes LE  |
| `INTEGER`      | `i32`             | 4 bytes LE  |
| `BIGINT`       | `i64`             | 8 bytes LE  |
| `CHAR(n)`      | `String` (fixed)  | n bytes     |
| `VARCHAR(n)`   | `String` (variable) | variable |
| `DOUBLE`       | `f64`             | 8 bytes LE  |
| `TIMESTAMP`    | `String`          | fixed-length (`YYYY-MM-DD HH:MM:SS.nnnnnnnnn UTC`) |

## Catalog Cache (`cache.rs`)

The `CatalogCache` is built once at startup from the loaded `Catalog` and
stays permanently resident in memory. It serves two purposes:

1. **O(1) metadata lookups** — HashMap indexes replace linear scans over `Vec`.
2. **Pre-materialized rows** — Each catalog table's data is converted to
   `Vec<Vec<Value>>` once, eliminating per-query struct→Value conversion.

### Architecture

```
load_catalog()          CatalogCache::new(catalog)
     │                           │
     ▼                           ▼
  Catalog ─────────────► CatalogCache
  (Vec<Table>,            ├─ catalog: Catalog           (typed access)
   Vec<Column>,           ├─ tables_data: HashMap       (schema,table) → CachedTable
   Vec<Tablespace>,       ├─ table_idx: HashMap         (schema,table) → index
   Vec<Schema>,           ├─ tablespace_by_id: HashMap  tbspaceid → index
   Vec<BufferPool>)       ├─ schema_idx: HashMap        schema name → index
                          └─ columns_by_table: HashMap  (schema,table) → sorted cols
```

### CachedTable

Each catalog table is pre-materialized into a `CachedTable`:

| Field | Type | Purpose |
|-------|------|---------|
| `column_names` | `Vec<String>` | Ordered column names |
| `column_index` | `HashMap<String, usize>` | Column name → position (O(1)) |
| `rows` | `Vec<Vec<Value>>` | All rows as Values (built once) |

### O(1) Lookup Methods

| Method | Returns | Key |
|--------|---------|-----|
| `get_table(schema, name)` | `Option<&Table>` | (schema, table) |
| `get_columns(schema, table)` | `Option<&[Column]>` | (schema, table), sorted by ordinal |
| `get_tablespace_by_id(id)` | `Option<&Tablespace>` | tbspaceid |
| `get_table_data(schema, table)` | `Option<&CachedTable>` | (schema, table) |

### Before vs After

| Operation | Before | After |
|-----------|--------|-------|
| Table lookup by name | O(n) match on `table_ref.table` string | O(1) HashMap |
| Column name → index | O(k) `.position()` per reference | O(1) HashMap |
| Row materialization | O(rows) per query (clone + convert structs) | O(1) — pre-built |
| `load_table_data()` | 90-line match with 5 hardcoded arms | 4-line cache lookup |

### Design Rationale — Industry Comparison

RustDB uses **full pre-load** (SQLite-style): all catalog tables are loaded into
memory at startup and kept permanently resident. Production systems like
PostgreSQL, DB2, and Oracle use **on-demand caching** with LRU eviction instead.

| Aspect | RustDB | PostgreSQL (syscache) | DB2 (catalog cache) | Oracle (row cache) |
|--------|--------|----------------------|---------------------|--------------------|
| Loading | All tables at startup | On-demand per-entry | On-demand | On-demand |
| Eviction | Never | Invalidation messages | LRU (`CATALOGCACHE_SZ`) | LRU in Shared Pool |
| Storage path | Separate from buffer pool | Through buffer pool | Through buffer pool (SYSCATSPACE) | Through buffer cache |
| Granularity | Entire tables | Individual tuples | Parsed descriptors | Individual rows |
| Invalidation | None | `sinval` shared-memory messages | Automatic on DDL commit | DDL triggers flush |
| Concurrency | Single-threaded | Per-entry pins + refcounts | Latch-protected | Latches + pins |

**Why full pre-load is the right choice here:**

- **Catalog is small relative to data.** Even 1,000 tables × 10 columns ≈ 3 MB.
  This is trivial compared to the GB-scale data the buffer pool manages.
- **Zero latency** — no cache-miss path, no miss-fill storms after restarts.
- **Simpler code** — no eviction, reference counting, or miss-fill logic.
- **Deterministic memory** — exact catalog memory usage is known at startup.

The on-demand approach in PostgreSQL/DB2 exists because of constraints we do not
have yet: schemas with hundreds of thousands of tables, shared memory across
multiple backend processes, and concurrent DDL requiring fine-grained invalidation.

**Why no eviction — even at scale:**

Target scale is up to 10K tables (typical: ~1,000 tables, ~12 columns each).
At that scale the entire catalog cache is roughly ~5 MB — trivially small.
Since this cache holds only catalog metadata (never user data), it cannot grow
unboundedly: the size is dictated by DDL, not by workload. Eviction would
add LRU tracking overhead to every lookup and create contention under
multi-threaded access — cost with no benefit when the entire working set fits
comfortably in memory.

| Scale | SYSTABLES | SYSCOLUMNS | HashMap overhead | Total |
|-------|-----------|------------|------------------|-------|
| 100 tables × 12 cols | 20 KB | 360 KB | 100 KB | ~500 KB |
| 1,000 tables × 12 cols | 200 KB | 3.6 MB | 1 MB | ~5 MB |
| 10,000 tables × 12 cols | 2 MB | 36 MB | 10 MB | ~50 MB |

**TODO — Multi-threaded access:**

- Wrap cache in `Arc<RwLock<CatalogCache>>` for concurrent read access.
- Query threads take read locks (zero contention — `RwLock` allows concurrent readers).
- DDL acquires a write lock to mutate entries (rare operation).
- No eviction machinery needed — all threads share the full resident cache.

**TODO — Zone Maps (SYSCOLUMNS extension):**

Add per-column zone-map metadata to `SYSCOLUMNS` so the executor can skip pages
that cannot satisfy a WHERE predicate without fetching them into the buffer pool.

New columns:
- `ZONEMAP` (`CHAR(1)`) — `Y`/`N` flag: is a zone map maintained for this column?

The actual per-page min/max values live in `TableFileInfo.zone_maps` in the
tablespace manager (in-memory `Vec<PageZoneMap>`), not in the catalog. The
catalog column just controls which columns have zone maps enabled.

On insert/update, the tablespace manager updates the page's min/max for each
zone-mapped column. On scan with a range predicate (`>`, `<`, `BETWEEN`),
the executor checks the zone map to skip pages whose range doesn't overlap.

**When to revisit the full-preload decision:**

- **100K+ tables** where memory cost is no longer negligible → add LRU eviction.
- **Concurrent DDL** (`CREATE TABLE`, `ALTER TABLE`) → add entry-level cache mutation.
- **Phase 5** (migrate catalog to slotted pages) will naturally move catalog data
  into the buffer pool, aligning with DB2/PostgreSQL architecture.

### Tests (6)

- `lookup_table_by_name` — O(1) table metadata access
- `lookup_tablespace_by_id` — O(1) by numeric ID
- `lookup_columns_sorted` — columns returned in ordinal order
- `cached_table_data_matches` — pre-materialized data matches expectations
- `cached_table_not_found` — missing table returns `None`
- `schema_lookup` — schema index populated
