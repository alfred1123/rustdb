# catalog/

System catalog module — manages the database's internal metadata.

## Files

| File           | Purpose                                                    |
|----------------|------------------------------------------------------------|
| `types.rs`     | Struct definitions: `Tablespace`, `Schema`, `Table`, `Column`, `Catalog` |
| `row.rs`       | `RowReader` / `RowWriter` — binary serialization using u64 LE length-prefixed fields |
| `config.rs`    | `DbConfig` — reads/writes the `admin/SQLDBCONF` database configuration file |
| `bootstrap.rs` | Creates a fresh database directory with `SQLDBCONF` and system catalog `.DAT` files |
| `loader.rs`    | Reads `.DAT` files from disk and deserializes them into `Catalog` |

## How It Works

1. **Bootstrap** (`bootstrap.rs`) writes `admin/SQLDBCONF` and the four system tables (`SYSTABLESPACES`, `SYSSCHEMAS`, `SYSTABLES`, `SYSCOLUMNS`) into `systbsp/` as `.DAT` files.
2. **Config** (`config.rs`) reads `SQLDBCONF` on subsequent startups so the engine knows the database's settings.
3. **Loader** (`loader.rs`) reads those `.DAT` files back, splitting them into rows and deserializing each row with `RowReader`.
3. The row format is simple: each field is `[u64 LE length][value bytes]`. Types like `SMALLINT` and `INTEGER` are stored as little-endian fixed-width integers; strings are raw UTF-8 bytes; booleans are `Y`/`N` flag bytes.

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
