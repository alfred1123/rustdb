# sql/

SQL parser, planner, and executor.

## Files

| File          | Purpose                                             |
|---------------|-----------------------------------------------------|
| `parser.rs`   | Parses SQL strings via `sqlparser` (generic dialect) |
| `types.rs`    | `Value` enum, `ResultSet`, `TableRef`               |
| `executor.rs` | Executes SELECT queries against the catalog         |

## Supported SQL

- `SELECT * FROM <table>` — all columns
- `SELECT col1, col2 FROM <table>` — specific columns
- `SELECT ... FROM schema.table` — schema-qualified table names
- `SELECT ... WHERE col = value` — equality filter
- `SELECT ... WHERE col != value` — inequality filter
- `WHERE ... AND ...` / `WHERE ... OR ...` — logical combinators
- String literals (`'value'`) and numeric literals in WHERE clauses

## Planned

- Query planner producing a logical plan
- Volcano-style iterator executor
- INSERT, UPDATE, DELETE statements
- Expressions in SELECT list (functions, arithmetic)
- ORDER BY, GROUP BY, HAVING
- JOINs

## SQLSTATE Error Codes

Errors follow the ANSI SQL SQLSTATE convention (5-character codes):

| Code  | Meaning                      | Example trigger                      |
|-------|------------------------------|--------------------------------------|
| 42601 | Parse error                  | `SELEC * FORM table`                 |
| 42000 | Syntax error                 | Invalid table reference              |
| 42S02 | Table not found              | `SELECT * FROM NONEXISTENT`          |
| 42S22 | Column not found             | `SELECT bogus FROM SYSTABLESPACES`   |
| 0A000 | Feature not supported        | `DELETE FROM ...`, JOINs             |
| 22000 | Data exception               | Unsupported literal type             |
