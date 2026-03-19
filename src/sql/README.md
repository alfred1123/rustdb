# sql/

SQL parser, planner, and executor.

## Files

| File          | Purpose                                             |
|---------------|-----------------------------------------------------|
| `parser.rs`   | Parses SQL strings via `sqlparser` (generic dialect) |
| `types.rs`    | `Value` enum, `ResultSet`, `TableRef`               |
| `executor.rs` | Executes SELECT, INSERT, DELETE via TablespaceManager |

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

## Data Path

All SQL statements go through the **TablespaceManager** for data I/O.
Column metadata comes from the **CatalogCache** (types, names, ordinals).

```
                  ┌─────────────┐
   SQL ──parse──▶ │  Executor   │
                  └──┬──────┬───┘
                     │      │
        metadata     │      │  data I/O
                     ▼      ▼
              CatalogCache  TablespaceManager
                            └──▶ BufferPool ──▶ Disk
```

Row deserialization is generic: column typename drives which `RowReader`
method is called (SMALLINT→read_i16, INTEGER→read_i32, CHAR/VARCHAR→read_string).
INSERT serialization works the same way in reverse via `RowWriter`.

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
- [ ] `UPDATE` statement

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
| 0A000 | Feature not supported              | `UPDATE ...`, JOINs, unsupported expressions |
