# RustDB

A transactional relational database engine written from scratch in Rust, following IBM DB2-style catalog and tablespace conventions.

## Project Structure

```
rustdb/
├── src/
│   ├── main.rs               # Entry point — CLI bootstrap + SQL REPL
│   ├── error.rs              # Error types (thiserror)
│   ├── catalog/              # System catalog (tables, columns, schemas, tablespaces)
│   ├── storage/              # Page-based storage engine
│   ├── sql/                  # SQL parser, planner, executor
│   ├── transaction/          # WAL, concurrency, recovery
│   └── server/               # TCP listener, wire protocol
├── TESTDB/                   # Default database instance directory
├── Cargo.toml
└── README.md
```

## Quick Start

```sh
# Build
cargo build

# Run tests
cargo test

# Bootstrap and start with a new database
cargo run -- --data-dir ./data/MYDB

# Use existing database directory
cargo run -- --data-dir ./data/TESTDB

# Text mode — writes human-readable TSV instead of binary (for debugging)
cargo run -- --data-dir ./data/DEBUGDB --text-mode

# Verbose logging (debug level)
RUST_LOG=debug cargo run -- --data-dir ./MYDB
```

## System Catalog

RustDB stores metadata in four system tables under the `RQSYS` schema:

| Table              | Purpose                            |
|--------------------|------------------------------------|
| SYSTABLESPACES     | Tablespace metadata                |
| SYSSCHEMAS         | Schema definitions                 |
| SYSTABLES          | Table metadata                     |
| SYSCOLUMNS         | Column definitions                 |

Catalog data files use the naming convention `SCHEMA.TABLE.FILEID.DAT` and are stored in the `systbsp/` directory.

## Binary Row Format

Each `.DAT` file contains rows serialized as:

```
[u64 LE row_length][row_bytes]
```

Each row is a sequence of length-prefixed fields:

```
[u64 LE field_length][field_value_bytes]
```

## Dependencies

| Crate       | Purpose              |
|-------------|----------------------|
| `thiserror` | Library error types  |
| `anyhow`    | CLI error handling   |
| `log`       | Logging facade       |
| `env_logger`| Runtime log config   |
| `sqlparser` | SQL parsing          |
