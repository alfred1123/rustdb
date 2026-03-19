use std::collections::HashMap;

use sqlparser::ast::{
    Expr, SelectItem, SetExpr, Statement, TableFactor,
};

use crate::catalog::cache::CatalogCache;
use crate::error::{sql_error, Result, SqlState};
use crate::sql::types::{ResultSet, TableRef, Value};

/// Execute a parsed SQL statement against the catalog cache.
pub fn execute(stmt: &Statement, cache: &CatalogCache) -> Result<ResultSet> {
    match stmt {
        Statement::Query(query) => execute_query(query, cache),
        _ => Err(sql_error(
            SqlState::FeatureNotSupported,
            format!("unsupported statement: {stmt}"),
        )),
    }
}

fn execute_query(
    query: &sqlparser::ast::Query,
    cache: &CatalogCache,
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

    let table_ref = match &from.relation {
        TableFactor::Table { name, .. } => {
            let parts: Vec<&str> = name.0.iter().map(|i| i.as_ident().unwrap().value.as_str()).collect();
            match parts.len() {
                1 => TableRef::resolve(None, parts[0])?,
                2 => TableRef::resolve(Some(parts[0]), parts[1])?,
                _ => {
                    return Err(sql_error(
                        SqlState::SyntaxError,
                        format!("invalid table reference: {name}"),
                    ))
                }
            }
        }
        _ => return Err(sql_error(SqlState::FeatureNotSupported, "unsupported FROM clause")),
    };

    log::debug!("SELECT from {}.{}", table_ref.schema, table_ref.table);

    // O(1) lookup: get pre-materialized table data from cache.
    let cached = cache.get_table_data(&table_ref.schema, &table_ref.table)
        .ok_or_else(|| sql_error(
            SqlState::TableNotFound,
            format!("table {}.{} not found", table_ref.schema, table_ref.table),
        ))?;

    // Resolve SELECT list using O(1) column index.
    let (selected_columns, selected_indices) =
        resolve_select_list(&select.projection, &cached.column_names, &cached.column_index)?;

    // Apply WHERE filter.
    let filtered_rows = match &select.selection {
        Some(expr) => {
            let mut result = Vec::new();
            for row in &cached.rows {
                if eval_where(expr, &cached.column_index, row)? {
                    result.push(row.clone());
                }
            }
            result
        }
        None => cached.rows.clone(),
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
    use crate::sql::parser;

    fn test_cache() -> CatalogCache {
        use crate::catalog::types::*;
        let catalog = Catalog {
            tablespaces: vec![
                Tablespace {
                    tbspaceid: 1,
                    tbspace: "SYSTBSP".into(),
                    tbspacetype: "D".into(),
                    datatype: "A".into(),
                    pagesize: 4096,
                    state: "N".into(),
                    bufferpoolid: 1,
                },
            ],
            schemas: vec![Schema { name: "RQSYS".into() }],
            tables: vec![
                Table {
                    name: "SYSTABLESPACES".into(),
                    schemaname: "RQSYS".into(),
                    tbspaceid: 1,
                    colcount: 7,
                },
            ],
            columns: vec![
                Column {
                    name: "TBSPACEID".into(),
                    tabname: "SYSTABLESPACES".into(),
                    schemaname: "RQSYS".into(),
                    ordinal: 0,
                    typename: "INTEGER".into(),
                    nullable: false,
                },
                Column {
                    name: "TBSPACE".into(),
                    tabname: "SYSTABLESPACES".into(),
                    schemaname: "RQSYS".into(),
                    ordinal: 1,
                    typename: "VARCHAR(128)".into(),
                    nullable: false,
                },
            ],
            bufferpools: vec![
                BufferPool {
                    bpid: 1,
                    bpname: "RQDEFAULTBP".into(),
                    pagesize: 4096,
                    npages: 128,
                },
            ],
        };
        CatalogCache::new(catalog)
    }

    #[test]
    fn select_star_from_systablespaces() {
        let cache = test_cache();
        let stmts = parser::parse("SELECT * FROM SYSTABLESPACES").unwrap();
        let rs = execute(&stmts[0], &cache).unwrap();
        assert_eq!(rs.columns.len(), 7);
        assert_eq!(rs.rows.len(), 1);
    }

    #[test]
    fn select_specific_columns() {
        let cache = test_cache();
        let stmts = parser::parse("SELECT tbspace, tbspaceid FROM SYSTABLESPACES").unwrap();
        let rs = execute(&stmts[0], &cache).unwrap();
        assert_eq!(rs.columns, vec!["TBSPACE", "TBSPACEID"]);
        assert_eq!(rs.rows.len(), 1);
    }

    #[test]
    fn select_with_schema_prefix() {
        let cache = test_cache();
        let stmts =
            parser::parse("SELECT * FROM RQSYS.SYSTABLESPACES").unwrap();
        let rs = execute(&stmts[0], &cache).unwrap();
        assert_eq!(rs.rows.len(), 1);
    }

    #[test]
    fn select_with_where_eq() {
        let cache = test_cache();
        let stmts = parser::parse(
            "SELECT tbspace FROM SYSTABLESPACES WHERE tbspaceid = 1",
        )
        .unwrap();
        let rs = execute(&stmts[0], &cache).unwrap();
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0].to_string(), "SYSTBSP");
    }

    #[test]
    fn select_with_where_no_match() {
        let cache = test_cache();
        let stmts = parser::parse(
            "SELECT tbspace FROM SYSTABLESPACES WHERE tbspaceid = 99",
        )
        .unwrap();
        let rs = execute(&stmts[0], &cache).unwrap();
        assert_eq!(rs.rows.len(), 0);
    }

    #[test]
    fn select_with_string_where() {
        let cache = test_cache();
        let stmts = parser::parse(
            "SELECT * FROM SYSCOLUMNS WHERE tabname = 'SYSTABLESPACES'",
        )
        .unwrap();
        let rs = execute(&stmts[0], &cache).unwrap();
        assert_eq!(rs.rows.len(), 2);
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
        let cache = test_cache();
        let stmts = parser::parse("SELECT * FROM NONEXISTENT").unwrap();
        assert_sqlstate(execute(&stmts[0], &cache), SqlState::TableNotFound);
    }

    #[test]
    fn error_column_not_found() {
        let cache = test_cache();
        let stmts =
            parser::parse("SELECT bogus FROM SYSTABLESPACES").unwrap();
        assert_sqlstate(execute(&stmts[0], &cache), SqlState::ColumnNotFound);
    }

    #[test]
    fn error_column_not_found_in_where() {
        let cache = test_cache();
        let stmts = parser::parse(
            "SELECT * FROM SYSTABLESPACES WHERE bogus = 1",
        )
        .unwrap();
        assert_sqlstate(execute(&stmts[0], &cache), SqlState::ColumnNotFound);
    }

    #[test]
    fn error_unsupported_insert() {
        let cache = test_cache();
        let stmts = parser::parse(
            "INSERT INTO SYSTABLESPACES VALUES (4, 'X', 'D', 4096, 'N')",
        )
        .unwrap();
        assert_sqlstate(
            execute(&stmts[0], &cache),
            SqlState::FeatureNotSupported,
        );
    }

    #[test]
    fn error_unsupported_delete() {
        let cache = test_cache();
        let stmts =
            parser::parse("DELETE FROM SYSTABLESPACES WHERE tbspaceid = 1").unwrap();
        assert_sqlstate(
            execute(&stmts[0], &cache),
            SqlState::FeatureNotSupported,
        );
    }

    #[test]
    fn error_empty_input() {
        let err = parser::parse("");
        // Empty input parses to zero statements, which is valid
        assert!(err.is_ok());
        assert!(err.unwrap().is_empty());
    }
}
