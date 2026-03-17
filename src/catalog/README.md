# catalog/

System catalog module — manages the database's internal metadata.

## Files

| File           | Purpose                                                    |
|----------------|------------------------------------------------------------|
| `types.rs`     | Struct definitions: `Tablespace`, `Schema`, `Table`, `Column`, `Catalog` |
| `row.rs`       | `RowReader` / `RowWriter` — binary serialization using u64 LE length-prefixed fields |
| `bootstrap.rs` | Creates a fresh database directory with system catalog `.DAT` files |
| `loader.rs`    | Reads `.DAT` files from disk and deserializes them into `Catalog` |

## How It Works

1. **Bootstrap** (`bootstrap.rs`) writes the four system tables (`SYSTABLESPACES`, `SYSSCHEMAS`, `SYSTABLES`, `SYSCOLUMNS`) into `systbsp/` as `.DAT` files.
2. **Loader** (`loader.rs`) reads those `.DAT` files back, splitting them into rows and deserializing each row with `RowReader`.
3. The row format is simple: each field is `[u64 LE length][value bytes]`. Types like `SMALLINT` and `INTEGER` are stored as little-endian fixed-width integers; strings are raw UTF-8 bytes; booleans are `Y`/`N` flag bytes.

## Text Mode (`--text-mode`)

When the `--text-mode` flag is passed, both bootstrap and loader use **tab-separated (TSV)** format instead of binary. The `.DAT` files become human-readable with a header row, making it easy to inspect catalog data with `cat` or any text editor.

Both modes share the same per-table functions — the `text_mode` flag controls branching internally so data definitions are never duplicated.

## Data Representations

### Tablespace TYPE

| Code | Meaning                                                        |
|------|----------------------------------------------------------------|
| `D`  | **Data** — persistent table data (e.g., `SYSTBSP`, `USERTBSP`) |
| `T`  | **Temporary** — transient data for sorts, joins, etc. (e.g., `TEMPTBSP`) |
| `L`  | **Large** — LOB (large object) storage (planned)               |

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
