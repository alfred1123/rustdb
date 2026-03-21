# RQDB

A transactional relational database engine written from scratch in Rust,
following IBM DB2-style catalog and tablespace conventions and ANSI SQL
standards.

**Status:** Storage engine and full CRUD SQL are complete. CREATE TABLE, INSERT,
UPDATE, DELETE, and SELECT all work with WHERE filtering. Data persists across
restarts. Transactions and networking are in progress.

## Why RQDB?

### What It Does Well Today

- **Page-based storage from the ground up.** Slotted pages with CRC32
  checksums, a binary max-heap free-space map (FSM), heap files, and a
  pre-allocated buffer pool with readers–writer latching — the same
  architecture used by PostgreSQL, DB2, and Oracle.
- **Self-describing catalog.** Five system tables (`SYSTABLESPACES`,
  `SYSSCHEMAS`, `SYSTABLES`, `SYSCOLUMNS`, `SYSBUFFERPOOLS`) stored in the
  same slotted-page format as user data. The catalog bootstraps itself.
- **O(1) metadata lookups.** A dedicated in-memory catalog cache with HashMap
  indexes — no per-query linear scans or struct conversion.
- **O(log P) free-space search.** PostgreSQL-style binary max-heap FSM
  replaces naive linear scans for page allocation.
- **Buffer pool with named pools.** DB2-style: separate pools for data,
  indexes, LOBs, and temp — each with its own page size, capacity, and LRU
  eviction policy. All page I/O goes through the pool.
- **Memory-safe by default.** Written in safe Rust — no use-after-free, no
  buffer overflows, no data races. Zero `unsafe` blocks in the codebase.
- **Zero external runtime dependencies.** No JVM, no garbage collector, no
  language runtime. Single static binary, starts in milliseconds.
- **Full CRUD SQL.** `SELECT`, `INSERT`, `UPDATE`, `DELETE`, and
  `CREATE TABLE` — all with `WHERE` filtering, column projection, and
  schema-prefixed table references.
- **Data persistence.** All data is flushed to `.DAT` heap files on
  shutdown and reloaded on startup — user tables survive restarts.
- **Safe database management.** `CONNECT TO` and `CREATE DATABASE` commands
  prevent accidental data loss — connecting requires an existing database,
  creating requires a non-existent one.
- **Interactive SQL REPL** with immediate feedback and SQLSTATE error codes.

### Comparison with Other Databases

| Feature | RQDB | SQLite | PostgreSQL | DuckDB |
|---------|------|--------|------------|--------|
| **Language** | Rust (memory-safe) | C | C | C++ |
| **Storage model** | Slotted pages + buffer pool | B-tree pages | Slotted pages + buffer pool | Column-oriented |
| **Free-space tracking** | Binary max-heap FSM — O(log P) | B-tree internal | Per-page FSM — O(1) amortized | N/A (append-only) |
| **Page checksums** | CRC32 on every page | Optional (`PRAGMA integrity_check`) | Optional (data checksums) | Per-column group |
| **Catalog** | Self-describing system tables (DB2-style) | `sqlite_master` | `pg_catalog` | `information_schema` |
| **Buffer pool** | Named pools, per-tablespace routing | Single page cache | `shared_buffers` | Memory-mapped |
| **Deployment** | Single binary, no runtime | Single file, no runtime | Server + client + extensions | Single library |
| **Concurrency** | Single-session (multi-session planned) | File-level locking / WAL mode | Full MVCC | Single-writer |
| **SQL coverage** | SELECT, INSERT, UPDATE, DELETE, CREATE TABLE + WHERE | Full SQL | Full SQL + extensions | Full SQL (analytical) |
| **Transactions** | Planned (ARIES-style WAL) | WAL or journal | WAL + MVCC | WAL |
| **Codebase size** | ~7K lines | ~150K lines | ~1.5M lines | ~300K lines |

### When to Consider RQDB

- **Learning database internals.** The codebase is small and well-documented.
  Each module (`page.rs`, `heap.rs`, `pool.rs`, `fsm.rs`, `tablespace.rs`)
  maps 1:1 to a textbook concept. READMEs explain the design at every level.
- **Embedding a page-oriented engine in Rust.** If you want DB2/PostgreSQL-
  style storage semantics (slotted pages, buffer pool, tablespace routing)
  in a Rust-native library without C FFI.
- **Prototyping storage extensions.** The layered architecture makes it easy
  to experiment with new page formats, eviction policies, or index structures
  without touching unrelated code.

### When Not to Use RQDB (Yet)

- **Production workloads.** No WAL, no crash recovery, no transactions yet.
- **Multi-user access.** Single-session only — no TCP server or connection
  pooling.
- **Complex queries.** No JOINs, aggregations, or subqueries yet.
- **Large datasets.** Not benchmarked at scale. No indexes beyond sequential
  scan.

## Current Architecture

```
┌──────────────────────────────────────────────┐
│                 SQL REPL                     │
│          (main.rs — interactive shell)       │
├──────────────────────────────────────────────┤
│               SQL Layer                      │
│   Parser (sqlparser) → Executor (CRUD+DDL)   │
├──────────────────────────────────────────────┤
│            Catalog Cache                     │
│   O(1) HashMap lookups, pre-materialized     │
├──────────────────────────────────────────────┤
│          Tablespace Manager                  │
│   (schema, table) → buffer pool routing      │
├──────────────────────────────────────────────┤
│           Buffer Pool Manager                │
│   Named pools: RQDEFAULTBP, INDEXBP,         │
│   LOBBP, TEMPBP — LRU eviction, latching    │
├──────────────────────────────────────────────┤
│              Heap Files                      │
│   .DAT files + FSM (.FSM binary max-heap)    │
├──────────────────────────────────────────────┤
│            Slotted Pages                     │
│   24B header, slot directory, CRC32          │
└──────────────────────────────────────────────┘
```

## Project Structure

```
rqdb/
├── src/
│   ├── main.rs               # Entry point — CLI + SQL REPL
│   ├── db.rs                 # Database lifecycle (open_database, create_database)
│   ├── error.rs              # Error types (thiserror)
│   ├── catalog/              # System catalog, bootstrap, loader, cache
│   ├── storage/              # Slotted pages, heap files, FSM, buffer pool, tablespace manager
│   ├── sql/                  # SQL parser + executor (SELECT, INSERT, UPDATE, DELETE, CREATE TABLE)
│   ├── transaction/          # WAL, concurrency, recovery (planned)
│   └── server/               # TCP listener, wire protocol (planned)
├── data/                     # Database instances (e.g. data/MYDB/)
├── Cargo.toml
└── README.md
```

## Quick Start

### Installation

```sh
# Build from source
cargo build

# Install the rqdb binary (places it in ~/.cargo/bin/)
cargo install --path .

# Verify it works
rqdb --help
```

> **Note:** `~/.cargo/bin` must be on your PATH. This is set up automatically by
> `rustup`. If `rqdb: command not found` after install, run:
> ```sh
> echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc
> source ~/.bashrc
> ```

### Running

```sh
# Start the REPL (no database connected)
rqdb

# Create a new database and connect
rqdb create database MYDB

# Connect to an existing database
rqdb connect to MYDB

# Override the base directory for databases (default: ./data)
rqdb --data-dir /tmp connect to MYDB

# Text mode — writes human-readable TSV instead of binary (for debugging)
rqdb --text-mode create database DEBUGDB

# Verbose logging (debug level)
RUST_LOG=debug rqdb connect to MYDB

# Run tests (130+ tests)
cargo test
```

You can also use `cargo run --` instead of `rqdb` without installing:

```sh
cargo run -- connect to MYDB
```

### Sample Session

```
$ rqdb
RQDB — interactive SQL shell
Type SQL queries, CONNECT TO <db>, CREATE DATABASE <db>, DISCONNECT, or \q to quit.

rqdb> CREATE DATABASE MYDB
Database MYDB created.

rqdb:MYDB> CREATE TABLE employees (id INTEGER NOT NULL, name VARCHAR(50), dept VARCHAR(30))

rqdb:MYDB> INSERT INTO employees VALUES (1, 'Alice', 'Engineering')
rqdb:MYDB> INSERT INTO employees VALUES (2, 'Bob', 'Marketing'), (3, 'Carol', 'Engineering')

rqdb:MYDB> SELECT name, dept FROM employees WHERE dept = 'Engineering'

rqdb:MYDB> UPDATE employees SET dept = 'Sales' WHERE id = 2

rqdb:MYDB> DELETE FROM employees WHERE id = 3

rqdb:MYDB> CONNECT TO OTHERDB
Connected to OTHERDB.

rqdb:OTHERDB> DISCONNECT
Disconnected from OTHERDB.

rqdb> \q
```

### Database Management Commands

| Command | CLI | REPL | Description |
|---------|:---:|:----:|-------------|
| `CONNECT TO <DBNAME>` | yes | yes | Connect to an existing database (error if not found) |
| `CREATE DATABASE <DBNAME>` | yes | yes | Create a new database and connect to it (error if exists) |
| `DISCONNECT` | — | yes | Flush and disconnect from the current database |

Database paths resolve to `<data-dir>/<DBNAME>` (default: `./data/<DBNAME>`).

### System Catalog Queries

```sql
SELECT * FROM RQSYS.SYSTABLES
SELECT NAME, TYPENAME, NULLABLE FROM RQSYS.SYSCOLUMNS WHERE TABNAME = 'EMPLOYEES'
SELECT TBSPACE, PAGESIZE, TBSPACETYPE FROM RQSYS.SYSTABLESPACES
SELECT * FROM RQSYS.SYSBUFFERPOOLS
```

## System Catalog

RQDB stores metadata in five system tables under the `RQSYS` schema:

| Table              | Purpose                            |
|--------------------|------------------------------------|
| SYSTABLESPACES     | Tablespace metadata (id, name, type, page size, buffer pool) |
| SYSSCHEMAS         | Schema definitions                 |
| SYSTABLES          | Table metadata (name, schema, tablespace, column count) |
| SYSCOLUMNS         | Column definitions (name, type, ordinal, nullable) |
| SYSBUFFERPOOLS     | Buffer pool definitions (name, page size, capacity) |

All catalog data is stored in slotted-page `.DAT` files using the naming
convention `SCHEMA.TABLE.FILEID.DAT` in the `systbsp/` directory. Each
`.DAT` file is accompanied by a `.FSM` free-space map.

## Storage Format

Each `.DAT` file is a sequence of fixed-size slotted pages (default 4096 bytes).
Each page contains a 24-byte header, a slot directory growing forward, and row
data growing backward. Pages are checksummed with CRC32.

Within each row, fields are encoded as length-prefixed values:

```
[u64 LE field_length][field_value_bytes]
```

Types: `SMALLINT` (2B LE), `INTEGER` (4B LE), `BIGINT` (8B LE), `VARCHAR(n)`
(variable UTF-8), `CHAR(n)` (fixed UTF-8), `DOUBLE` (8B LE), `TIMESTAMP`
(33B UTF-8).

## Dependencies

| Crate        | Version | Purpose                |
|-------------|---------|------------------------|
| `thiserror`  | 2       | Library error types    |
| `anyhow`     | 1       | CLI error handling     |
| `log`        | 0.4     | Logging facade         |
| `env_logger` | 0.11    | Runtime log config     |
| `sqlparser`  | 0.55    | SQL parsing            |
| `crc32fast`  | 1       | Page checksums         |

## Roadmap

- [x] Slotted pages with CRC32 checksums
- [x] Heap file manager with RID addressing
- [x] Binary max-heap free-space map (FSM)
- [x] Pre-allocated buffer pool with LRU eviction and latching
- [x] Named buffer pools (DB2-style) with per-tablespace routing
- [x] Tablespace manager — central I/O coordinator
- [x] Self-describing system catalog (5 tables, page-based storage)
- [x] In-memory catalog cache with O(1) lookups
- [x] SQL REPL with SELECT + WHERE
- [x] INSERT / DELETE in SQL executor (via TablespaceManager)
- [x] UPDATE in SQL executor (in-place with row migration)
- [x] CREATE TABLE (DDL) with auto-schema creation and catalog registration
- [x] Data persistence across restart (flush + reload)
- [x] SQLSTATE error codes for all SQL errors
- [x] CONNECT TO / CREATE DATABASE / DISCONNECT (safe database management)
- [x] DROP TABLE (DDL)
- [ ] Write-ahead log (WAL) with ARIES-style recovery
- [ ] MVCC or lock-based concurrency control
- [ ] B-tree indexes
- [ ] TCP server with wire protocol
- [ ] Multi-session support

## License

See [LICENSE](LICENSE) for details.
