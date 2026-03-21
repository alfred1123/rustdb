# sql/

SQL parser, planner, and executor.

## Files

| File          | Purpose                                             |
|---------------|-----------------------------------------------------|
| `parser.rs`   | Parses SQL strings via `sqlparser` (generic dialect) |
| `types.rs`    | `Value` enum, `ResultSet`, `TableRef`               |
| `executor.rs` | Executes SELECT, INSERT, UPDATE, DELETE via TablespaceManager |

## Supported SQL

### SELECT

- `SELECT * FROM <table>` — all columns
- `SELECT col1, col2 FROM <table>` — specific columns
- `SELECT ... FROM schema.table` — schema-qualified table names
- `SELECT ... WHERE col = value` — equality filter
- `SELECT ... WHERE col != value` — inequality filter
- `WHERE ... AND ...` / `WHERE ... OR ...` — logical combinators
- String literals (`'value'`) and numeric literals in WHERE clauses

### INSERT

- `INSERT INTO <table> VALUES (v1, v2, ...)` — insert a row (all columns)
- `INSERT INTO <table> (c1, c2) VALUES (v1, v2)` — insert with explicit column list
- `INSERT INTO <table> VALUES (...), (...), (...)` — multiple rows

### DELETE

- `DELETE FROM <table>` — delete all rows
- `DELETE FROM <table> WHERE col = value` — conditional delete

### UPDATE

- `UPDATE <table> SET col = value` — update all rows
- `UPDATE <table> SET col = value WHERE condition` — conditional update
- `UPDATE <table> SET c1 = v1, c2 = v2 ...` — update multiple columns
- SET expressions can reference columns (`SET col = other_col`) or use literals
- Uses delete-then-insert strategy (no in-place update) to reuse existing storage paths

## Data Path

All SQL statements go through the **TablespaceManager** for data I/O.
Column metadata comes from the **CatalogCache** (types, names, ordinals).
Column name→index maps are precomputed in the cache via `get_column_meta()`
so the executor never rebuilds them per query.

```
                  ┌─────────────┐
   SQL ──parse──▶ │  Executor   │
                  └──┬──────┬───┘
                     │      │
        metadata     │      │  data I/O
                     ▼      ▼
              CatalogCache  TablespaceManager
              ├─ get_columns()       └──▶ BufferPool ──▶ Disk
              └─ get_column_meta()
```

Row deserialization is generic: column typename drives which `RowReader`
method is called (SMALLINT→read_i16, INTEGER→read_i32, CHAR/VARCHAR→read_string).
INSERT and UPDATE serialization works the same way in reverse via `RowWriter`.

## Planned

### Near-term — SQL Coverage Expansion

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

### Medium-term

- [ ] `LIKE` pattern matching in WHERE
- [ ] `IN (list)` expression in WHERE
- [ ] `BETWEEN` expression in WHERE
- [ ] Arithmetic expressions (`+`, `-`, `*`, `/`) in SELECT and WHERE
- [ ] `DISTINCT` keyword
- [ ] `CREATE TABLE` / `DROP TABLE` (DDL)

### Longer-term

- [ ] Query planner producing a logical plan
- [ ] Volcano-style iterator executor
- [ ] `GROUP BY` + aggregate functions (`SUM`, `AVG`, `MIN`, `MAX`, `COUNT`)
- [ ] `HAVING` clause
- [ ] JOINs (nested-loop)
- [ ] Subqueries

## TODO — Multi-threaded Executor

The executor currently takes `&CatalogCache` (single-threaded borrow). When
multi-session is added, change to `Arc<RwLock<CatalogCache>>` and acquire a
shared read lock for the duration of query execution. DDL statements will
need a write lock to mutate the cache. No cache eviction is needed at our
target scale (≤10K tables).

## SQLSTATE Error Codes

Errors follow the ANSI SQL SQLSTATE convention (5-character codes):

| Code  | Meaning                            | Example trigger                              |
|-------|------------------------------------|----------------------------------------------|
| 21S01 | Insert value list mismatch         | `INSERT INTO t VALUES (1)` when t has 3 cols |
| 22000 | Data exception (general)           | Unsupported literal type                     |
| 22003 | Numeric value out of range         | Number too large for target column type      |
| 22005 | Error in assignment (type mismatch)| `INSERT INTO t (int_col) VALUES ('abc')`     |
| 23502 | NOT NULL violation                 | `INSERT` with NULL for non-nullable column   |
| 42000 | Syntax error                       | Invalid table reference, empty identifier    |
| 42601 | Parse error                        | `SELEC * FORM table`                         |
| 42S02 | Table not found                    | `SELECT * FROM NONEXISTENT`                  |
| 42S22 | Column not found                   | `SELECT bogus FROM SYSTABLESPACES`           |
| 0A000 | Feature not supported              | JOINs, unsupported expressions               |

## Complexity Analysis (Big O)

### Variables

| Symbol | Meaning |
|--------|---------|
| N | Total rows in the table |
| P | Total pages in the heap file |
| S | Slots per page |
| C | Number of columns in the table |
| K | Number of SET assignments (UPDATE only) |
| M | Number of rows matched by WHERE |

### INSERT — O(C + log P) per row

**Call chain:**

```
execute_insert()
  ├─ cache.get_columns()            O(1)   — HashMap lookup
  ├─ column ordering loop           O(C)
  └─ per row in VALUES:
      ├─ eval_literal() × C         O(C)
      ├─ serialize_row()            O(C)   — RowWriter iterates columns
      └─ tsm.insert_row()
          ├─ fsm.search(needed)     O(log P) — binary max-heap descent
          ├─ pool.fetch_page_mut()  O(1)*  — buffer pool hash lookup
          ├─ page.insert_row()      O(S)   — scan slot directory
          ├─ pool.unpin()           O(1)
          └─ fsm.update()           O(log P) — sift-up in binary heap
```

\* O(1) amortized; worst-case O(P) on eviction + flush.

If no page qualifies: `pool.new_page()` O(1) append + `fsm.extend()` O(1) amortized.

**Total per row: O(C + log P)**

### DELETE — O(N·C + M·log P)

**Call chain:**

```
execute_delete()
  ├─ cache.get_columns()             O(1)
  ├─ TableMeta::from_columns()       O(C)   — builds HashMap
  ├─ tsm.table_scan()               O(N)   — full heap scan
  │    └─ per page (P pages):
  │        ├─ pool.fetch_page()      O(1)*
  │        ├─ read all S slots       O(S)
  │        └─ pool.unpin()           O(1)
  ├─ per scanned row (N rows):
  │    ├─ deserialize_row()          O(C)   — RowReader per column
  │    └─ eval_where()               O(1)   — HashMap column lookup + comparison
  └─ per matched row (M rows):
       └─ tsm.delete_row()
            ├─ pool.fetch_page_mut() O(1)*
            ├─ page.delete_row()     O(1)   — zero slot entry
            ├─ pool.unpin()          O(1)
            └─ fsm.update()          O(log P)
```

**Total: O(N·C + M·log P)**

### UPDATE — O(N·C + M·(C + log P))

**Call chain:**

```
execute_update()
  ├─ cache.get_columns()             O(1)
  ├─ TableMeta::from_columns()       O(C)
  ├─ resolve SET assignments         O(K)   — HashMap lookups
  ├─ tsm.table_scan()               O(N)   — full heap scan
  ├─ per scanned row (N rows):
  │    ├─ deserialize_row()          O(C)
  │    ├─ eval_where()               O(1)
  │    └─ if matched:
  │         ├─ eval_expr() × K       O(K)   — apply SET assignments
  │         └─ serialize_row()       O(C)
  └─ per matched row (M rows):
       ├─ tsm.delete_row()          O(1 + log P) — same as DELETE
       └─ tsm.insert_row()          O(C + log P) — same as INSERT
```

**Total: O(N·C + M·(C + log P))**

### Summary

| Operation | Complexity | Dominant Cost |
|-----------|------------|---------------|
| INSERT (1 row) | O(C + log P) | FSM search + serialize |
| DELETE (with WHERE) | O(N·C + M·log P) | Full table scan |
| UPDATE (with WHERE) | O(N·C + M·(C + log P)) | Full table scan + re-serialize |

### Comparison with Industry-Standard Databases

Industry databases (PostgreSQL, MySQL/InnoDB, IBM DB2) achieve dramatically
better performance through indexes, in-place updates, and query planning.
Below is a side-by-side comparison:

| Aspect | RustDB (current) | Industry Standard (PostgreSQL, DB2, etc.) |
|--------|-------------------|-------------------------------------------|
| **SELECT with WHERE** | O(N·C) — full table scan, no indexes | O(log N) with B-tree index; O(1) with hash index |
| **INSERT** | O(C + log P) — FSM-based page search | O(C + log N) per index — similar base cost, but each index adds O(log N) for key insertion |
| **DELETE with WHERE** | O(N·C + M·log P) — full scan to find rows | O(log N + M·log N) with index — scan avoidance is the key win |
| **UPDATE with WHERE** | O(N·C + M·(C + log P)) — full scan + delete/insert | O(log N + M) with index + HOT (heap-only tuple) in-place update |
| **UPDATE strategy** | Delete old row + insert new row | In-place update (DB2/InnoDB) or HOT update (PostgreSQL) — avoids rewriting indexes |
| **Concurrency** | Single-threaded, page-level latches | MVCC with row-level locks; thousands of concurrent transactions |
| **Query planning** | None — direct execution | Cost-based optimizer choosing between seq scan, index scan, bitmap scan, etc. |
| **WHERE evaluation** | Scans all rows, then filters | Pushes predicates into index lookups; only touches qualifying rows |
| **JOIN support** | None | Nested loop, hash join, merge join, with cost-based selection |
| **Write-ahead log** | WAL infrastructure exists but not wired to DML | Full WAL-logged DML; crash recovery replays log to restore consistency |
| **Buffer pool** | LRU eviction with FSM | Clock-sweep (PostgreSQL) or LRU variants with adaptive prefetching |

**The critical gap is the O(N) full table scan for DELETE/UPDATE.** Industry
databases solve this with B-tree indexes that reduce row lookup from O(N) to
O(log N). Without indexes, every DELETE and UPDATE must read every row in the
table regardless of how selective the WHERE clause is.

### Development Required to Reach Industry Standard

The following items are ordered by impact — each one closes a major gap:

#### 1. B-tree Index (highest impact)

Reduces WHERE-based lookups from O(N) to O(log N). This is the single
largest performance gap. Requires:

- [ ] On-disk B-tree page structure (internal nodes + leaf nodes)
- [ ] `CREATE INDEX` / `DROP INDEX` DDL
- [ ] Index catalog table (`SYSINDEXES`) for metadata
- [ ] Index maintenance on INSERT/DELETE/UPDATE (keep tree balanced)
- [ ] Executor integration: choose index scan vs. sequential scan

With a B-tree index on a WHERE column:

| Operation | Current | With B-tree Index |
|-----------|---------|-------------------|
| DELETE WHERE col = X | O(N·C) | O(log N + C) |
| UPDATE WHERE col = X | O(N·C) | O(log N + C) |
| SELECT WHERE col = X | O(N·C) | O(log N + C) |

#### 2. In-place UPDATE (avoid delete + insert)

Current UPDATE deletes the old row and inserts a new one, which:
- Writes twice to the page layer (one delete, one insert)
- May relocate the row to a different page
- Would invalidate any index pointers (once indexes exist)

Industry approach:
- **DB2/InnoDB:** Overwrite the row in place if it fits in the same slot
- **PostgreSQL HOT:** Write a new tuple version on the same page, avoid index update if indexed columns didn't change

Requires:
- [ ] `page.update_row(slot, new_data)` — in-place overwrite when new data fits
- [ ] Fallback to delete+insert when new row is larger than old slot
- [ ] FSM update after in-place modification

#### 3. Query Planner / Optimizer

Currently the executor directly interprets the AST. Industry databases
compile SQL into a logical plan, optimize it, then produce a physical plan:

```
SQL → Parse → Logical Plan → Optimize → Physical Plan → Execute
              (current)                  (missing)
```

Requires:
- [ ] Logical plan representation (scan, filter, project, join nodes)
- [ ] Rule-based optimizations (predicate pushdown, projection pruning)
- [ ] Cost-based optimizer (estimate cardinality, choose access paths)
- [ ] Volcano-style iterator model for physical operators

#### 4. WAL-logged DML

The WAL infrastructure exists in the transaction module but is not connected
to INSERT/UPDATE/DELETE. Without WAL logging, committed changes can be lost
on crash.

Requires:
- [ ] Log records for INSERT, DELETE, UPDATE operations
- [ ] Write log record before modifying data page (write-ahead contract)
- [ ] ARIES-style recovery: redo/undo on crash restart
- [ ] Transaction begin/commit/rollback protocol

#### 5. MVCC / Row-level Concurrency

Current model: single-threaded execution with page-level latches. Industry
databases support thousands of concurrent read/write transactions.

Requires:
- [ ] Transaction IDs and row version stamps
- [ ] MVCC visibility checks (each row carries xmin/xmax)
- [ ] Snapshot isolation (read a consistent point-in-time view)
- [ ] Row-level locking (shared/exclusive) for write conflicts
- [ ] Deadlock detection

#### 6. Additional SQL Features

To be usable as a general-purpose SQL engine:

- [ ] `ORDER BY`, `LIMIT`, `OFFSET`
- [ ] `GROUP BY` + aggregates (`SUM`, `AVG`, `MIN`, `MAX`, `COUNT`)
- [ ] JOINs (nested-loop first, then hash join and merge join)
- [ ] Subqueries and CTEs
- [ ] `CREATE TABLE` / `DROP TABLE`
- [ ] `ALTER TABLE`
- [ ] Constraints (`PRIMARY KEY`, `FOREIGN KEY`, `UNIQUE`, `CHECK`)
- [ ] `NULL` handling (`IS NULL`, three-valued logic)

### Maturity Scale

| Level | Description | RustDB Status |
|-------|-------------|---------------|
| 1. Storage | Page-based heap storage with buffer pool | ✅ Complete |
| 2. Catalog | Self-describing system catalog | ✅ Complete |
| 3. Basic DML | SELECT, INSERT, UPDATE, DELETE on single tables | ✅ Complete |
| 4. Indexes | B-tree or hash indexes for O(log N) lookups | ❌ Not started |
| 5. WAL-logged DML | Crash-safe mutations via write-ahead log | ❌ Infrastructure only |
| 6. Query planner | Cost-based optimizer with multiple access paths | ❌ Not started |
| 7. Transactions | ACID transactions with MVCC concurrency | ❌ Not started |
| 8. DDL | CREATE/DROP/ALTER TABLE, CREATE INDEX | ❌ Not started |
| 9. Advanced SQL | JOINs, aggregates, subqueries, ORDER BY | ❌ Not started |
| 10. Production | Connection pooling, auth, replication | ❌ Not started |

RustDB is at **Level 3** — functional single-table DML with a solid storage
foundation. The next major milestone is **Level 4 (B-tree indexes)**, which
would close the biggest performance gap versus industry databases.
