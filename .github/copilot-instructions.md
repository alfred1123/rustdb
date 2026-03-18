# RustDB — Copilot Instructions

## Project Overview

RustDB is a transactional relational database engine written from scratch in Rust.
It follows IBM DB2-style catalog and tablespace conventions.
RustDB shall follow the **ANSI SQL** industry standard for SQL syntax, semantics, and data types.

**Schema prefix:** `RQSYS` (system catalog schema)

## Directory Layout

```
rustdb/
├── .github/                  # CI and Copilot configuration
├── src/
│   ├── main.rs               # Entry point — CLI / server bootstrap
│   ├── catalog/              # System catalog (SYSTABLES, SYSCOLUMNS, SYSSCHEMAS, SYSTABLESPACES)
│   ├── storage/              # Page-based storage engine, tablespace manager, buffer pool
│   ├── sql/                  # SQL parser, planner, executor
│   ├── transaction/          # WAL, MVCC / lock-based concurrency, ARIES recovery
│   └── server/               # TCP listener, wire protocol, session management
├── TESTDB/                   # Default database instance directory
│   ├── admin/                # Database configuration
│   ├── backups/              # Database backups
│   ├── log/                  # Write-ahead log (WAL) files
│   ├── systbsp/              # System tablespace — catalog data files
│   ├── temptbsp/             # Temporary tablespace
│   └── usertbsp/             # User tablespace — user table data files
├── Cargo.toml
└── README.md
```

## Data File Naming Convention

Files in tablespaces use IBM-style naming:
```
<SCHEMA>.<TABLENAME>.<FILEID>.DAT
```
Example: `RQSYS.SYSTABLES.0.DAT`

## System Catalog Tables (in `systbsp/`)

| Table                  | Purpose                             |
|------------------------|-------------------------------------|
| RQSYS.SYSTABLESPACES   | Tablespace metadata (tbspaceid, tbspace, tbspacetype, datatype, pagesize, state) |
| RQSYS.SYSTABLES        | Table metadata (name, schemaname, tbspaceid, colcount) |
| RQSYS.SYSCOLUMNS       | Column definitions (name, tabname, schemaname, ordinal, typename, nullable) |
| RQSYS.SYSSCHEMAS       | Schema definitions                  |

All catalog data is stored in a custom binary row format with length-prefixed fields.

## Binary Row Format (current)

Each row is serialized as a sequence of typed fields. Each field starts with:
- **length prefix** (u64 LE) — byte length of the value
- **value bytes** — the raw data

Types observed: `SMALLINT`, `INTEGER`, `BIGINT`, `CHAR(n)`, `VARCHAR(n)`, timestamps as fixed-length strings (`YYYY-MM-DD HH:MM:SS.nnnnnnnnn UTC`).

Nullable fields use a sentinel `N`/`Y` flag byte.

## Build & Run

```sh
# Build
cargo build

# Run tests
cargo test

# Run database server (planned)
cargo run -- --data-dir ./TESTDB
```

## Coding Conventions

- **Language:** rustc 1.94.0 (4a4ef493e 2026-03-02)
- **Error handling:** Use `thiserror` for library errors, `anyhow` for binary.
  All I/O must return `Result<T, E>` — no `.unwrap()` in library code.
- **Unsafe:** Avoid `unsafe` unless required for memory-mapped I/O or low-level page ops.
  Every `unsafe` block must have a `// SAFETY:` comment.
- **Naming:** snake_case for functions/variables, CamelCase for types, SCREAMING_SNAKE for constants.
- **Catalog identifiers:** Always uppercase (e.g., `RQSYS`, `SYSTABLES`).
- **Page size default:** 4096 bytes (configurable per tablespace).
- **Testing:** Unit tests in `#[cfg(test)]` modules; integration tests in `tests/`.
- **Logging:** Use the `log` crate (`log::info!`, `log::debug!`, `log::warn!`, `log::error!`).
  Do not use `println!` for operational messages — reserve `println!` for user-facing output only.
  Log levels: `error` for failures, `warn` for recoverable issues, `info` for key milestones,
  `debug` for detailed internals. Control at runtime via `RUST_LOG` env var (default: `info`).

## Architecture Principles

1. **Storage engine** is page-oriented. Buffer pool mediates all page I/O.
2. **Catalog is self-describing.** The catalog tables are stored as regular tables
   in the system tablespace and bootstrapped on database creation.
3. **WAL-first.** Every mutation writes to the WAL before the data page.
   Bootstrap is exempt — it runs outside the WAL since there is no prior state to recover.
4. **ACID transactions** via WAL + ARIES-style recovery.
5. **SQL layer** is separate from storage — uses a Volcano-style iterator model.
6. **Strict page-level latch model.** Buffer pool frames enforce readers–writer
   exclusion: shared reads allow multiple readers but block writers; exclusive
   writes block all other access. No uncommitted reads. This is the strict ACID
   default. Uncommitted-read (`READ UNCOMMITTED` isolation) may be added later
   as a localised relaxation of the latch check — the enum and guard structure
   are designed for that extension.

## Key Dependencies (planned)

| Crate           | Purpose                        |
|-----------------|--------------------------------|
| `sqlparser`     | SQL parsing (PostgreSQL dialect) |
| `thiserror`     | Error types                    |
| `anyhow`        | CLI error handling             |
| `log`           | Logging facade                 |
| `env_logger`    | Runtime log configuration      |
| `bytes`         | Buffer manipulation            |
| `tokio`         | Async I/O (server)             |
| `serde`         | (De)serialization for config   |
| `crc32fast`     | Page checksums                 |

## Common Pitfalls

- The existing `.DAT` files are **little-endian** binary. Do not assume text encoding.
- Tablespace IDs are globally unique integers — never hardcode.
- Catalog must be loaded before any user query can proceed.
- Buffer pool eviction must respect dirty-page WAL flush (write-ahead contract).
- **Do not duplicate logic into separate functions for different modes** (e.g., text vs binary).
  Instead, branch within a single function so data definitions and field mappings exist in one place.
  Separate functions means changes must be made in two places, which leads to drift and bugs.
- **Update the relevant README when adding or changing features.**
  Each module directory (`src/catalog/`, `src/storage/`, `src/sql/`, etc.) and `TESTDB/`
  have their own `README.md`. When you add or change behavior in a module, update that
  module's README — not the root README — with data representations, new flags, format
  changes, or usage notes. The root `README.md` covers project-level overview and quick start only.
