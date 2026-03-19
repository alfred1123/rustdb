use std::collections::HashMap;

use sqlparser::ast::{
    Expr, SelectItem, SetExpr, Statement, TableFactor,
};

use crate::catalog::cache::CatalogCache;
use crate::catalog::row::{RowReader, RowWriter};
use crate::catalog::types::Column;
use crate::error::{sql_error, Result, SqlState};
use crate::sql::types::{ResultSet, TableRef, Value};
use crate::storage::tablespace::TablespaceManager;

/// Execute a parsed SQL statement against the storage engine.
pub fn execute(
    stmt: &Statement,
    cache: &CatalogCache,
    tsm: &mut TablespaceManager,
) -> Result<ResultSet> {
    match stmt {
        Statement::Query(query) => execute_query(query, cache, tsm),
        Statement::Insert(insert) => execute_insert(insert, cache, tsm),
        Statement::Delete(delete) => execute_delete(delete, cache, tsm),
        _ => Err(sql_error(
            SqlState::FeatureNotSupported,
            format!("unsupported statement: {stmt}"),
        )),
    }
}

fn execute_query(
    query: &sqlparser::ast::Query,
    cache: &CatalogCache,
    tsm: &mut TablespaceManager,
) -> Result<ResultSet> {
    let select = match query.body.as_ref() {
        SetExpr::Select(s) => s,
        _ => return Err(sql_error(SqlState::FeatureNotSupported, "only SELECT is supported")),
    };

    // Resolve FROM clause — exactly one table.
    if select.from.len() != 1 {
        return Err(sql_error(
            SqlState::SyntaxError,
            "exactly one table in FROM clause is required",
        ));
    }
    let from = &select.from[0];
    if !from.joins.is_empty() {
        return Err(sql_error(SqlState::FeatureNotSupported, "JOINs are not yet supported"));
    }

    let table_ref = resolve_table_factor(&from.relation)?;

    log::debug!("SELECT from {}.{}", table_ref.schema, table_ref.table);

    // Get column metadata from catalog cache.
    let columns = cache
        .get_columns(&table_ref.schema, &table_ref.table)
        .ok_or_else(|| {
            sql_error(
                SqlState::TableNotFound,
                format!("table {}.{} not found", table_ref.schema, table_ref.table),
            )
        })?;

    let meta = TableMeta::from_columns(columns);

    // Scan rows from storage via tablespace manager.
    let raw_rows = tsm.table_scan(&table_ref.schema, &table_ref.table)?;
    let all_rows: Vec<Vec<Value>> = raw_rows
        .iter()
        .map(|(_, bytes)| deserialize_row(bytes, columns))
        .collect::<Result<_>>()?;

    // Resolve SELECT list using O(1) column index.
    let (selected_columns, selected_indices) =
        resolve_select_list(&select.projection, &meta.column_names, &meta.column_index)?;

    // Apply WHERE filter.
    let filtered_rows = match &select.selection {
        Some(expr) => {
            let mut result = Vec::new();
            for row in &all_rows {
                if eval_where(expr, &meta.column_index, row)? {
                    result.push(row.clone());
                }
            }
            result
        }
        None => all_rows,
    };

    // Project selected columns.
    let projected: Vec<Vec<Value>> = filtered_rows
        .iter()
        .map(|row| selected_indices.iter().map(|&i| row[i].clone()).collect())
        .collect();

    Ok(ResultSet {
        columns: selected_columns,
        rows: projected,
    })
}

// ── INSERT ──

fn execute_insert(
    insert: &sqlparser::ast::Insert,
    cache: &CatalogCache,
    tsm: &mut TablespaceManager,
) -> Result<ResultSet> {
    let table_ref = match &insert.table {
        sqlparser::ast::TableObject::TableName(name) => resolve_table_name(name)?,
        _ => return Err(sql_error(SqlState::FeatureNotSupported, "table functions not supported")),
    };
    log::debug!("INSERT into {}.{}", table_ref.schema, table_ref.table);

    let columns = cache
        .get_columns(&table_ref.schema, &table_ref.table)
        .ok_or_else(|| {
            sql_error(
                SqlState::TableNotFound,
                format!("table {}.{} not found", table_ref.schema, table_ref.table),
            )
        })?;

    // Determine column ordering: explicit column list or table-order.
    let col_order = if !insert.columns.is_empty() {
        // Map provided column names to their ordinal positions.
        let meta = TableMeta::from_columns(columns);
        insert.columns
            .iter()
            .map(|ident| {
                let name = ident.value.to_uppercase();
                meta.column_index.get(&name).copied().ok_or_else(|| {
                    sql_error(SqlState::ColumnNotFound, format!("column {name} not found"))
                })
            })
            .collect::<Result<Vec<usize>>>()?
    } else {
        (0..columns.len()).collect()
    };

    // Extract value rows from the INSERT source.
    let body = insert.source.as_ref().ok_or_else(|| {
        sql_error(SqlState::SyntaxError, "INSERT requires a VALUES clause")
    })?;
    let rows_ast = match body.body.as_ref() {
        SetExpr::Values(values) => &values.rows,
        _ => {
            return Err(sql_error(
                SqlState::FeatureNotSupported,
                "only INSERT ... VALUES is supported",
            ))
        }
    };

    let mut inserted = 0i32;
    for row_exprs in rows_ast {
        if row_exprs.len() != col_order.len() {
            return Err(sql_error(
                SqlState::InsertValueListMismatch,
                format!(
                    "expected {} values, got {}",
                    col_order.len(),
                    row_exprs.len()
                ),
            ));
        }

        // Build a Value vec in table-column order, filling with defaults.
        let mut values = vec![Value::Null; columns.len()];
        for (val_idx, &col_idx) in col_order.iter().enumerate() {
            values[col_idx] = eval_literal(&row_exprs[val_idx])?;
        }

        let row_bytes = serialize_row(&values, columns)?;
        tsm.insert_row(&table_ref.schema, &table_ref.table, &row_bytes)?;
        inserted += 1;
    }

    Ok(ResultSet {
        columns: vec!["ROWS_INSERTED".into()],
        rows: vec![vec![Value::Integer(inserted)]],
    })
}

// ── DELETE ──

fn execute_delete(
    delete: &sqlparser::ast::Delete,
    cache: &CatalogCache,
    tsm: &mut TablespaceManager,
) -> Result<ResultSet> {
    // Resolve table from the FROM clause.
    let from_tables = match &delete.from {
        sqlparser::ast::FromTable::WithFromKeyword(tables) => tables,
        sqlparser::ast::FromTable::WithoutKeyword(tables) => tables,
    };
    if from_tables.len() != 1 {
        return Err(sql_error(
            SqlState::SyntaxError,
            "exactly one table in DELETE FROM is required",
        ));
    }
    let table_ref = resolve_table_factor(&from_tables[0].relation)?;
    log::debug!("DELETE from {}.{}", table_ref.schema, table_ref.table);

    let columns = cache
        .get_columns(&table_ref.schema, &table_ref.table)
        .ok_or_else(|| {
            sql_error(
                SqlState::TableNotFound,
                format!("table {}.{} not found", table_ref.schema, table_ref.table),
            )
        })?;

    let meta = TableMeta::from_columns(columns);

    // Scan all rows, evaluate WHERE, collect RIDs to delete.
    let raw_rows = tsm.table_scan(&table_ref.schema, &table_ref.table)?;
    let mut rids_to_delete = Vec::new();
    for (rid, bytes) in &raw_rows {
        let row = deserialize_row(bytes, columns)?;
        let matches = match &delete.selection {
            Some(expr) => eval_where(expr, &meta.column_index, &row)?,
            None => true, // DELETE without WHERE deletes all rows
        };
        if matches {
            rids_to_delete.push(*rid);
        }
    }

    let mut deleted = 0i32;
    for rid in rids_to_delete {
        if tsm.delete_row(&table_ref.schema, &table_ref.table, rid)? {
            deleted += 1;
        }
    }

    Ok(ResultSet {
        columns: vec!["ROWS_DELETED".into()],
        rows: vec![vec![Value::Integer(deleted)]],
    })
}

// ── Table reference helpers ──

fn resolve_table_factor(relation: &TableFactor) -> Result<TableRef> {
    match relation {
        TableFactor::Table { name, .. } => resolve_table_name(name),
        _ => Err(sql_error(SqlState::FeatureNotSupported, "unsupported FROM clause")),
    }
}

fn resolve_table_name(name: &sqlparser::ast::ObjectName) -> Result<TableRef> {
    let parts: Vec<&str> = name
        .0
        .iter()
        .map(|i| i.as_ident().unwrap().value.as_str())
        .collect();
    match parts.len() {
        1 => TableRef::resolve(None, parts[0]),
        2 => TableRef::resolve(Some(parts[0]), parts[1]),
        _ => Err(sql_error(
            SqlState::SyntaxError,
            format!("invalid table reference: {name}"),
        )),
    }
}

// ── Generic row (de)serialization ──

/// Column metadata extracted from catalog for the executor.
struct TableMeta {
    column_names: Vec<String>,
    column_index: HashMap<String, usize>,
}

impl TableMeta {
    fn from_columns(columns: &[Column]) -> Self {
        let column_names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        let column_index: HashMap<String, usize> = column_names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), i))
            .collect();
        Self {
            column_names,
            column_index,
        }
    }
}

/// Deserialize raw row bytes into Values using column type metadata.
fn deserialize_row(bytes: &[u8], columns: &[Column]) -> Result<Vec<Value>> {
    let mut reader = RowReader::new(bytes);
    let mut values = Vec::with_capacity(columns.len());
    for col in columns {
        let base_type = col.typename.split('(').next().unwrap_or(&col.typename);
        let val = match base_type {
            "SMALLINT" => Value::SmallInt(reader.read_i16()?),
            "INTEGER" => Value::Integer(reader.read_i32()?),
            "CHAR" | "VARCHAR" => Value::Str(reader.read_string()?),
            other => {
                return Err(sql_error(
                    SqlState::FeatureNotSupported,
                    format!("unsupported column type: {other}"),
                ))
            }
        };
        values.push(val);
    }
    Ok(values)
}

/// Serialize a row of Values into binary bytes using column type metadata.
fn serialize_row(values: &[Value], columns: &[Column]) -> Result<Vec<u8>> {
    let mut writer = RowWriter::new();
    for (val, col) in values.iter().zip(columns.iter()) {
        let base_type = col.typename.split('(').next().unwrap_or(&col.typename);
        match (val, base_type) {
            (Value::SmallInt(v), "SMALLINT") => writer.write_i16(*v),
            (Value::Integer(v), "INTEGER") => writer.write_i32(*v),
            (Value::Str(v), "CHAR" | "VARCHAR") => writer.write_string(v),
            // Auto-coerce integer → SMALLINT/INTEGER
            (Value::Integer(v), "SMALLINT") => writer.write_i16(*v as i16),
            (Value::SmallInt(v), "INTEGER") => writer.write_i32(*v as i32),
            (Value::BigInt(v), "INTEGER") => writer.write_i32(*v as i32),
            (Value::BigInt(v), "SMALLINT") => writer.write_i16(*v as i16),
            (Value::Null, _) => {
                return Err(sql_error(
                    SqlState::NotNullViolation,
                    format!("NULL not allowed for column {}", col.name),
                ))
            }
            _ => {
                return Err(sql_error(
                    SqlState::AssignmentError,
                    format!(
                        "type mismatch for column {}: cannot store {} as {}",
                        col.name, val, col.typename
                    ),
                ))
            }
        }
    }
    Ok(writer.finish())
}

/// Evaluate a literal expression to a Value (for INSERT VALUES).
fn eval_literal(expr: &Expr) -> Result<Value> {
    match expr {
        Expr::Value(val) => match &val.value {
            sqlparser::ast::Value::SingleQuotedString(s) => Ok(Value::Str(s.clone())),
            sqlparser::ast::Value::Number(n, _) => {
                if let Ok(v) = n.parse::<i16>() {
                    Ok(Value::SmallInt(v))
                } else if let Ok(v) = n.parse::<i32>() {
                    Ok(Value::Integer(v))
                } else if let Ok(v) = n.parse::<i64>() {
                    Ok(Value::BigInt(v))
                } else {
                    Err(sql_error(SqlState::DataException, format!("unsupported number: {n}")))
                }
            }
            sqlparser::ast::Value::Null => Ok(Value::Null),
            _ => Err(sql_error(
                SqlState::DataException,
                format!("unsupported literal: {val}"),
            )),
        },
        Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Minus,
            expr: inner,
        } => {
            let v = eval_literal(inner)?;
            match v {
                Value::SmallInt(n) => Ok(Value::SmallInt(-n)),
                Value::Integer(n) => Ok(Value::Integer(-n)),
                Value::BigInt(n) => Ok(Value::BigInt(-n)),
                _ => Err(sql_error(SqlState::DataException, "cannot negate non-numeric value")),
            }
        }
        _ => Err(sql_error(
            SqlState::FeatureNotSupported,
            format!("expression not allowed in VALUES: {expr}"),
        )),
    }
}

/// Resolve the SELECT list to column names and their indices.
/// Uses the column_index HashMap for O(1) name→index resolution.
fn resolve_select_list(
    projection: &[SelectItem],
    all_columns: &[String],
    column_index: &HashMap<String, usize>,
) -> Result<(Vec<String>, Vec<usize>)> {
    let mut names = Vec::new();
    let mut indices = Vec::new();

    for item in projection {
        match item {
            SelectItem::Wildcard(_) => {
                for (i, col) in all_columns.iter().enumerate() {
                    names.push(col.clone());
                    indices.push(i);
                }
            }
            SelectItem::UnnamedExpr(Expr::Identifier(ident)) => {
                let col_name = ident.value.to_uppercase();
                let idx = *column_index.get(&col_name).ok_or_else(|| {
                    sql_error(SqlState::ColumnNotFound, format!("column {col_name} not found"))
                })?;
                names.push(col_name);
                indices.push(idx);
            }
            SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) => {
                // Handle table.column or schema.table.column — take last part.
                let col_name = parts
                    .last()
                    .map(|i| i.value.to_uppercase())
                    .ok_or_else(|| sql_error(SqlState::SyntaxError, "empty identifier"))?;
                let idx = *column_index.get(&col_name).ok_or_else(|| {
                    sql_error(SqlState::ColumnNotFound, format!("column {col_name} not found"))
                })?;
                names.push(col_name);
                indices.push(idx);
            }
            _ => {
                return Err(sql_error(
                    SqlState::FeatureNotSupported,
                    format!("unsupported SELECT item: {item}"),
                ))
            }
        }
    }

    Ok((names, indices))
}

/// Evaluate a WHERE expression against a row. Supports simple comparisons.
/// Uses column_index HashMap for O(1) column resolution.
fn eval_where(
    expr: &Expr,
    column_index: &HashMap<String, usize>,
    row: &[Value],
) -> Result<bool> {
    match expr {
        Expr::BinaryOp { left, op, right } => {
            use sqlparser::ast::BinaryOperator;
            match op {
                BinaryOperator::And => {
                    Ok(eval_where(left, column_index, row)?
                        && eval_where(right, column_index, row)?)
                }
                BinaryOperator::Or => {
                    Ok(eval_where(left, column_index, row)?
                        || eval_where(right, column_index, row)?)
                }
                BinaryOperator::Eq => {
                    let l = eval_expr(left, column_index, row)?;
                    let r = eval_expr(right, column_index, row)?;
                    Ok(values_eq(&l, &r))
                }
                BinaryOperator::NotEq => {
                    let l = eval_expr(left, column_index, row)?;
                    let r = eval_expr(right, column_index, row)?;
                    Ok(!values_eq(&l, &r))
                }
                _ => Err(sql_error(
                    SqlState::FeatureNotSupported,
                    format!("unsupported operator: {op}"),
                )),
            }
        }
        _ => Err(sql_error(
            SqlState::FeatureNotSupported,
            format!("unsupported WHERE expression: {expr}"),
        )),
    }
}

/// Evaluate a scalar expression to a Value.
/// Uses column_index HashMap for O(1) column resolution.
fn eval_expr(
    expr: &Expr,
    column_index: &HashMap<String, usize>,
    row: &[Value],
) -> Result<Value> {
    match expr {
        Expr::Identifier(ident) => {
            let name = ident.value.to_uppercase();
            let idx = *column_index.get(&name).ok_or_else(|| {
                sql_error(SqlState::ColumnNotFound, format!("column {name} not found"))
            })?;
            Ok(row[idx].clone())
        }
        Expr::CompoundIdentifier(parts) => {
            let name = parts
                .last()
                .map(|i| i.value.to_uppercase())
                .ok_or_else(|| sql_error(SqlState::SyntaxError, "empty identifier"))?;
            let idx = *column_index.get(&name).ok_or_else(|| {
                sql_error(SqlState::ColumnNotFound, format!("column {name} not found"))
            })?;
            Ok(row[idx].clone())
        }
        Expr::Value(val) => match &val.value {
            sqlparser::ast::Value::SingleQuotedString(s) => {
                Ok(Value::Str(s.clone()))
            }
            sqlparser::ast::Value::Number(n, _) => {
                if let Ok(v) = n.parse::<i16>() {
                    Ok(Value::SmallInt(v))
                } else if let Ok(v) = n.parse::<i32>() {
                    Ok(Value::Integer(v))
                } else if let Ok(v) = n.parse::<i64>() {
                    Ok(Value::BigInt(v))
                } else {
                    Err(sql_error(SqlState::DataException, format!("unsupported number: {n}")))
                }
            }
            _ => Err(sql_error(
                SqlState::DataException,
                format!("unsupported literal: {val}"),
            )),
        },
        _ => Err(sql_error(
            SqlState::FeatureNotSupported,
            format!("unsupported expression: {expr}"),
        )),
    }
}

/// Compare two Values for equality.
fn values_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::SmallInt(x), Value::SmallInt(y)) => x == y,
        (Value::Integer(x), Value::Integer(y)) => x == y,
        (Value::BigInt(x), Value::BigInt(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        // Cross-type numeric comparisons.
        (Value::SmallInt(x), Value::Integer(y)) => *x as i32 == *y,
        (Value::Integer(x), Value::SmallInt(y)) => *x == *y as i32,
        (Value::SmallInt(x), Value::BigInt(y)) => *x as i64 == *y,
        (Value::BigInt(x), Value::SmallInt(y)) => *x == *y as i64,
        (Value::Integer(x), Value::BigInt(y)) => *x as i64 == *y,
        (Value::BigInt(x), Value::Integer(y)) => *x == *y as i64,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::cache::CatalogCache;
    use crate::catalog::config::DbConfig;
    use crate::sql::parser;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("rustdb_exec_{name}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Bootstrap a temp database and return (cache, tsm, _dir_guard).
    fn test_fixture(name: &str) -> (CatalogCache, TablespaceManager, TempDir) {
        let dir = TempDir::new(name);
        let cfg = DbConfig::default();
        crate::catalog::bootstrap::bootstrap(&dir.0, &cfg).unwrap();
        let catalog =
            crate::catalog::loader::load_catalog(&dir.0, false, cfg.page_size).unwrap();
        let cache = CatalogCache::new(catalog);
        let tsm = TablespaceManager::open(&dir.0, &cache).unwrap();
        (cache, tsm, dir)
    }

    #[test]
    fn select_star_from_systablespaces() {
        let (cache, mut tsm, _dir) = test_fixture("sel_star");
        let stmts = parser::parse("SELECT * FROM SYSTABLESPACES").unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.columns.len(), 7);
        assert_eq!(rs.rows.len(), 3); // 3 tablespaces bootstrapped
    }

    #[test]
    fn select_specific_columns() {
        let (cache, mut tsm, _dir) = test_fixture("sel_cols");
        let stmts = parser::parse("SELECT tbspace, tbspaceid FROM SYSTABLESPACES").unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.columns, vec!["TBSPACE", "TBSPACEID"]);
        assert_eq!(rs.rows.len(), 3);
    }

    #[test]
    fn select_with_schema_prefix() {
        let (cache, mut tsm, _dir) = test_fixture("sel_schema");
        let stmts =
            parser::parse("SELECT * FROM RQSYS.SYSTABLESPACES").unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 3);
    }

    #[test]
    fn select_with_where_eq() {
        let (cache, mut tsm, _dir) = test_fixture("sel_where");
        let stmts = parser::parse(
            "SELECT tbspace FROM SYSTABLESPACES WHERE tbspaceid = 1",
        )
        .unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0].to_string(), "SYSTBSP");
    }

    #[test]
    fn select_with_where_no_match() {
        let (cache, mut tsm, _dir) = test_fixture("sel_nomatch");
        let stmts = parser::parse(
            "SELECT tbspace FROM SYSTABLESPACES WHERE tbspaceid = 99",
        )
        .unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 0);
    }

    #[test]
    fn select_with_string_where() {
        let (cache, mut tsm, _dir) = test_fixture("sel_str");
        let stmts = parser::parse(
            "SELECT * FROM SYSCOLUMNS WHERE tabname = 'SYSTABLESPACES'",
        )
        .unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 7); // 7 columns in SYSTABLESPACES
    }

    #[test]
    fn select_all_catalog_tables() {
        let (cache, mut tsm, _dir) = test_fixture("sel_all");

        // SYSTABLES: 5 tables
        let stmts = parser::parse("SELECT * FROM SYSTABLES").unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 5);

        // SYSSCHEMAS: 1 schema
        let stmts = parser::parse("SELECT * FROM SYSSCHEMAS").unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0].to_string(), "RQSYS");

        // SYSBUFFERPOOLS: 4 pools
        let stmts = parser::parse("SELECT * FROM SYSBUFFERPOOLS").unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 4);
    }

    // ── INSERT tests ──

    #[test]
    fn insert_and_select() {
        let (cache, mut tsm, _dir) = test_fixture("ins_sel");

        // Insert a new tablespace row.
        let stmts = parser::parse(
            "INSERT INTO SYSTABLESPACES VALUES (10, 'NEWTBSP', 'D', 'A', 4096, 'N', 1)",
        )
        .unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "1"); // 1 row inserted

        // Verify it's there via SELECT.
        let stmts = parser::parse(
            "SELECT tbspace FROM SYSTABLESPACES WHERE tbspaceid = 10",
        )
        .unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0].to_string(), "NEWTBSP");
    }

    #[test]
    fn insert_with_column_list() {
        let (cache, mut tsm, _dir) = test_fixture("ins_cols");

        let stmts = parser::parse(
            "INSERT INTO SYSSCHEMAS (NAME) VALUES ('USERSCH')",
        )
        .unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "1");

        let stmts = parser::parse("SELECT * FROM SYSSCHEMAS").unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 2); // RQSYS + USERSCH
    }

    #[test]
    fn insert_multiple_rows() {
        let (cache, mut tsm, _dir) = test_fixture("ins_multi");

        let stmts = parser::parse(
            "INSERT INTO SYSSCHEMAS VALUES ('S1'), ('S2'), ('S3')",
        )
        .unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "3");

        let stmts = parser::parse("SELECT * FROM SYSSCHEMAS").unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 4); // RQSYS + S1 + S2 + S3
    }

    // ── DELETE tests ──

    #[test]
    fn delete_with_where() {
        let (cache, mut tsm, _dir) = test_fixture("del_where");

        // 3 tablespaces exist. Delete TEMPTBSP (id=3).
        let stmts = parser::parse(
            "DELETE FROM SYSTABLESPACES WHERE tbspaceid = 3",
        )
        .unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "1");

        let stmts = parser::parse("SELECT * FROM SYSTABLESPACES").unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 2); // SYSTBSP + USERTBSP remain
    }

    #[test]
    fn delete_all() {
        let (cache, mut tsm, _dir) = test_fixture("del_all");

        let stmts = parser::parse("DELETE FROM SYSSCHEMAS").unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "1"); // 1 schema deleted

        let stmts = parser::parse("SELECT * FROM SYSSCHEMAS").unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 0);
    }

    #[test]
    fn delete_no_match() {
        let (cache, mut tsm, _dir) = test_fixture("del_nomatch");

        let stmts = parser::parse(
            "DELETE FROM SYSTABLESPACES WHERE tbspaceid = 99",
        )
        .unwrap();
        let rs = execute(&stmts[0], &cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "0");
    }

    // ── Invalid SQL tests — verify SQLSTATE codes ──

    fn assert_sqlstate(result: Result<ResultSet>, expected: SqlState) {
        match result {
            Err(crate::error::Error::Sql { state, .. }) => assert_eq!(state, expected),
            Err(other) => panic!("expected SQLSTATE {expected}, got: {other}"),
            Ok(_) => panic!("expected error with SQLSTATE {expected}, got Ok"),
        }
    }

    #[test]
    fn error_parse_invalid_syntax() {
        let err = parser::parse("SELEC * FORM table");
        match err {
            Err(crate::error::Error::Sql { state, .. }) => {
                assert_eq!(state, SqlState::ParseError);
            }
            other => panic!("expected parse error, got: {other:?}"),
        }
    }

    #[test]
    fn error_table_not_found() {
        let (cache, mut tsm, _dir) = test_fixture("err_table");
        let stmts = parser::parse("SELECT * FROM NONEXISTENT").unwrap();
        assert_sqlstate(execute(&stmts[0], &cache, &mut tsm), SqlState::TableNotFound);
    }

    #[test]
    fn error_column_not_found() {
        let (cache, mut tsm, _dir) = test_fixture("err_col");
        let stmts =
            parser::parse("SELECT bogus FROM SYSTABLESPACES").unwrap();
        assert_sqlstate(execute(&stmts[0], &cache, &mut tsm), SqlState::ColumnNotFound);
    }

    #[test]
    fn error_column_not_found_in_where() {
        let (cache, mut tsm, _dir) = test_fixture("err_col_where");
        let stmts = parser::parse(
            "SELECT * FROM SYSTABLESPACES WHERE bogus = 1",
        )
        .unwrap();
        assert_sqlstate(execute(&stmts[0], &cache, &mut tsm), SqlState::ColumnNotFound);
    }

    #[test]
    fn error_unsupported_update() {
        let (cache, mut tsm, _dir) = test_fixture("err_update");
        let stmts = parser::parse(
            "UPDATE SYSTABLESPACES SET tbspace = 'X' WHERE tbspaceid = 1",
        )
        .unwrap();
        assert_sqlstate(
            execute(&stmts[0], &cache, &mut tsm),
            SqlState::FeatureNotSupported,
        );
    }

    #[test]
    fn error_empty_input() {
        let err = parser::parse("");
        assert!(err.is_ok());
        assert!(err.unwrap().is_empty());
    }

    #[test]
    fn error_insert_value_list_mismatch() {
        let (cache, mut tsm, _dir) = test_fixture("err_val_cnt");
        // SYSTABLESPACES has 7 columns but we provide only 2 values.
        let stmts = parser::parse(
            "INSERT INTO SYSTABLESPACES VALUES (1, 'X')",
        )
        .unwrap();
        assert_sqlstate(
            execute(&stmts[0], &cache, &mut tsm),
            SqlState::InsertValueListMismatch,
        );
    }

    #[test]
    fn error_not_null_violation() {
        let (cache, mut tsm, _dir) = test_fixture("err_null");
        // SYSSCHEMAS has 1 column (NAME VARCHAR). Insert NULL.
        let stmts = parser::parse(
            "INSERT INTO SYSSCHEMAS VALUES (NULL)",
        )
        .unwrap();
        assert_sqlstate(
            execute(&stmts[0], &cache, &mut tsm),
            SqlState::NotNullViolation,
        );
    }

    #[test]
    fn error_type_mismatch() {
        let (cache, mut tsm, _dir) = test_fixture("err_type");
        // SYSTABLESPACES first column is SMALLINT. Insert a string.
        let stmts = parser::parse(
            "INSERT INTO SYSTABLESPACES VALUES ('not_a_number', 'X', 'D', 'A', 4096, 'N', 1)",
        )
        .unwrap();
        assert_sqlstate(
            execute(&stmts[0], &cache, &mut tsm),
            SqlState::AssignmentError,
        );
    }
}
