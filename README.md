# RustDB

A transactional relational database engine written from scratch in Rust,
following IBM DB2-style catalog and tablespace conventions and ANSI SQL
standards.

**Status:** Storage engine complete. SQL reads work. Writes, transactions,
and networking are in progress.

## Why RustDB?

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
- **Interactive SQL REPL** with `SELECT`, `WHERE` filtering, column
  projection, and schema-prefixed table references.

### Comparison with Other Databases

| Feature | RustDB | SQLite | PostgreSQL | DuckDB |
|---------|--------|--------|------------|--------|
| **Language** | Rust (memory-safe) | C | C | C++ |
| **Storage model** | Slotted pages + buffer pool | B-tree pages | Slotted pages + buffer pool | Column-oriented |
| **Free-space tracking** | Binary max-heap FSM — O(log P) | B-tree internal | Per-page FSM — O(1) amortized | N/A (append-only) |
| **Page checksums** | CRC32 on every page | Optional (`PRAGMA integrity_check`) | Optional (data checksums) | Per-column group |
| **Catalog** | Self-describing system tables (DB2-style) | `sqlite_master` | `pg_catalog` | `information_schema` |
| **Buffer pool** | Named pools, per-tablespace routing | Single page cache | `shared_buffers` | Memory-mapped |
| **Deployment** | Single binary, no runtime | Single file, no runtime | Server + client + extensions | Single library |
| **Concurrency** | Single-session (multi-session planned) | File-level locking / WAL mode | Full MVCC | Single-writer |
| **SQL coverage** | SELECT, INSERT, DELETE + WHERE | Full SQL | Full SQL + extensions | Full SQL (analytical) |
| **Transactions** | Planned (ARIES-style WAL) | WAL or journal | WAL + MVCC | WAL |
| **Codebase size** | ~3K lines | ~150K lines | ~1.5M lines | ~300K lines |

### When to Consider RustDB

- **Learning database internals.** The codebase is small and well-documented.
  Each module (`page.rs`, `heap.rs`, `pool.rs`, `fsm.rs`, `tablespace.rs`)
  maps 1:1 to a textbook concept. READMEs explain the design at every level.
- **Embedding a page-oriented engine in Rust.** If you want DB2/PostgreSQL-
  style storage semantics (slotted pages, buffer pool, tablespace routing)
  in a Rust-native library without C FFI.
- **Prototyping storage extensions.** The layered architecture makes it easy
  to experiment with new page formats, eviction policies, or index structures
  without touching unrelated code.

### When Not to Use RustDB (Yet)

- **Production workloads.** No WAL, no crash recovery, no transactions yet.
- **Multi-user access.** Single-session only — no TCP server or connection
  pooling.
- **Complex queries.** No JOINs, aggregations, subqueries, or INSERT/UPDATE/
  DELETE in the SQL layer yet.
- **Large datasets.** Not benchmarked at scale. No indexes beyond sequential
  scan.

## Current Architecture

```
┌──────────────────────────────────────────────┐
│                 SQL REPL                     │
│          (main.rs — interactive shell)       │
├──────────────────────────────────────────────┤
│               SQL Layer                      │
│   Parser (sqlparser) → Executor (SELECT)     │
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
rustdb/
├── src/
│   ├── main.rs               # Entry point — CLI bootstrap + SQL REPL
│   ├── error.rs              # Error types (thiserror)
│   ├── catalog/              # System catalog, bootstrap, loader, cache
│   ├── storage/              # Slotted pages, heap files, FSM, buffer pool, tablespace manager
│   ├── sql/                  # SQL parser + executor
│   ├── transaction/          # WAL, concurrency, recovery (planned)
│   └── server/               # TCP listener, wire protocol (planned)
├── data/TESTDB/              # Default database instance directory
├── Cargo.toml
└── README.md
```

## Quick Start

```sh
# Build
cargo build

# Run tests (84 tests)
cargo test

# Bootstrap and start with a new database
cargo run -- --data-dir ./data/MYDB

# Use existing database directory
cargo run -- --data-dir ./data/TESTDB

# Text mode — writes human-readable TSV instead of binary (for debugging)
cargo run -- --data-dir ./data/DEBUGDB --text-mode

# Verbose logging (debug level)
RUST_LOG=debug cargo run -- --data-dir ./data/MYDB
```

### Sample Queries

```sql
-- All tables in the database
SELECT * FROM RQSYS.SYSTABLES

-- Column definitions for a specific table
SELECT NAME, TYPENAME, NULLABLE FROM RQSYS.SYSCOLUMNS WHERE TABNAME = 'SYSTABLESPACES'

-- Tablespace configuration
SELECT TBSPACE, PAGESIZE, TBSPACETYPE FROM RQSYS.SYSTABLESPACES

-- Buffer pool configuration
SELECT * FROM RQSYS.SYSBUFFERPOOLS

-- Filter by integer column
SELECT TBSPACE, PAGESIZE FROM RQSYS.SYSTABLESPACES WHERE TBSPACEID = 1
```

## System Catalog

RustDB stores metadata in five system tables under the `RQSYS` schema:

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
(variable UTF-8), `CHAR(n)` (fixed UTF-8), `DOUBLE` (8B LE).

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

### SQL Coverage Expansion

- [ ] Comparison operators in WHERE (`<`, `>`, `<=`, `>=`)
- [ ] `IS NULL` / `IS NOT NULL` in WHERE
- [ ] `NOT` operator in WHERE
- [ ] `BIGINT` type in row serialization/deserialization
- [ ] `BOOLEAN` type (`TRUE`/`FALSE` literals + serde)
- [ ] `COUNT(*)` aggregate (no GROUP BY)
- [ ] Column aliases (`SELECT col AS name`)
- [ ] `LIMIT` clause
- [ ] `OFFSET` clause
- [ ] `ORDER BY` clause
- [ ] `UPDATE` statement
- [ ] `LIKE` pattern matching in WHERE
- [ ] `IN (list)` expression in WHERE
- [ ] `BETWEEN` expression in WHERE
- [ ] Arithmetic expressions (`+`, `-`, `*`, `/`) in SELECT and WHERE
- [ ] `DISTINCT` keyword
- [ ] `CREATE TABLE` / `DROP TABLE` (DDL)
- [ ] `GROUP BY` + aggregate functions (`SUM`, `AVG`, `MIN`, `MAX`, `COUNT`)
- [ ] `HAVING` clause
- [ ] JOINs (nested-loop)
- [ ] Subqueries

### Infrastructure

- [ ] Write-ahead log (WAL) with ARIES-style recovery
- [ ] MVCC or lock-based concurrency control
- [ ] B-tree indexes
- [ ] TCP server with wire protocol
- [ ] Multi-session support

## License

See [LICENSE](LICENSE) for details.
