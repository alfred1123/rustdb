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

### Tests (6)

- `lookup_table_by_name` — O(1) table metadata access
- `lookup_tablespace_by_id` — O(1) by numeric ID
- `lookup_columns_sorted` — columns returned in ordinal order
- `cached_table_data_matches` — pre-materialized data matches expectations
- `cached_table_not_found` — missing table returns `None`
- `schema_lookup` — schema index populated
