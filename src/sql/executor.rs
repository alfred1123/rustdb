use sqlparser::ast::{
    Expr, SelectItem, SetExpr, Statement, TableFactor,
};

use crate::catalog::types::Catalog;
use crate::error::{sql_error, Result, SqlState};
use crate::sql::types::{ResultSet, TableRef, Value};

/// Execute a parsed SQL statement against the catalog.
pub fn execute(stmt: &Statement, catalog: &Catalog) -> Result<ResultSet> {
    match stmt {
        Statement::Query(query) => execute_query(query, catalog),
        _ => Err(sql_error(
            SqlState::FeatureNotSupported,
            format!("unsupported statement: {stmt}"),
        )),
    }
}

fn execute_query(
    query: &sqlparser::ast::Query,
    catalog: &Catalog,
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

    // Get all columns and rows for the resolved table.
    let (all_columns, all_rows) = load_table_data(catalog, &table_ref)?;

    // Resolve SELECT list.
    let (selected_columns, selected_indices) =
        resolve_select_list(&select.projection, &all_columns)?;

    // Apply WHERE filter.
    let filtered_rows = match &select.selection {
        Some(expr) => {
            let mut result = Vec::new();
            for row in &all_rows {
                if eval_where(expr, &all_columns, row)? {
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

/// Load all data for a catalog table into column names + row values.
fn load_table_data(
    catalog: &Catalog,
    table_ref: &TableRef,
) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    match table_ref.table.as_str() {
        "SYSTABLESPACES" => {
            let cols = vec![
                "TBSPACEID".into(), "TBSPACE".into(), "TBSPACETYPE".into(),
                "DATATYPE".into(), "PAGESIZE".into(), "STATE".into(),
                "BUFFERPOOLID".into(),
            ];
            let rows: Vec<Vec<Value>> = catalog
                .tablespaces
                .iter()
                .map(|ts| {
                    vec![
                        Value::Integer(ts.tbspaceid),
                        Value::Str(ts.tbspace.clone()),
                        Value::Str(ts.tbspacetype.clone()),
                        Value::Str(ts.datatype.clone()),
                        Value::Integer(ts.pagesize),
                        Value::Str(ts.state.clone()),
                        Value::Integer(ts.bufferpoolid),
                    ]
                })
                .collect();
            Ok((cols, rows))
        }
        "SYSSCHEMAS" => {
            let cols = vec!["NAME".into()];
            let rows: Vec<Vec<Value>> = catalog
                .schemas
                .iter()
                .map(|s| vec![Value::Str(s.name.clone())])
                .collect();
            Ok((cols, rows))
        }
        "SYSTABLES" => {
            let cols = vec![
                "NAME".into(), "SCHEMANAME".into(),
                "TBSPACEID".into(), "COLCOUNT".into(),
            ];
            let rows: Vec<Vec<Value>> = catalog
                .tables
                .iter()
                .map(|t| {
                    vec![
                        Value::Str(t.name.clone()),
                        Value::Str(t.schemaname.clone()),
                        Value::SmallInt(t.tbspaceid),
                        Value::SmallInt(t.colcount),
                    ]
                })
                .collect();
            Ok((cols, rows))
        }
        "SYSCOLUMNS" => {
            let cols = vec![
                "NAME".into(), "TABNAME".into(), "SCHEMANAME".into(),
                "ORDINAL".into(), "TYPENAME".into(), "NULLABLE".into(),
            ];
            let rows: Vec<Vec<Value>> = catalog
                .columns
                .iter()
                .map(|c| {
                    vec![
                        Value::Str(c.name.clone()),
                        Value::Str(c.tabname.clone()),
                        Value::Str(c.schemaname.clone()),
                        Value::SmallInt(c.ordinal),
                        Value::Str(c.typename.clone()),
                        Value::Bool(c.nullable),
                    ]
                })
                .collect();
            Ok((cols, rows))
        }
        "SYSBUFFERPOOLS" => {
            let cols = vec![
                "BPID".into(), "BPNAME".into(),
                "PAGESIZE".into(), "NPAGES".into(),
            ];
            let rows: Vec<Vec<Value>> = catalog
                .bufferpools
                .iter()
                .map(|bp| {
                    vec![
                        Value::Integer(bp.bpid),
                        Value::Str(bp.bpname.clone()),
                        Value::Integer(bp.pagesize),
                        Value::Integer(bp.npages),
                    ]
                })
                .collect();
            Ok((cols, rows))
        }
        _ => Err(sql_error(
            SqlState::TableNotFound,
            format!("table {}.{} not found", table_ref.schema, table_ref.table),
        )),
    }
}

/// Resolve the SELECT list to column names and their indices.
fn resolve_select_list(
    projection: &[SelectItem],
    all_columns: &[String],
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
                let idx = all_columns
                    .iter()
                    .position(|c| c == &col_name)
                    .ok_or_else(|| {
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
                let idx = all_columns
                    .iter()
                    .position(|c| c == &col_name)
                    .ok_or_else(|| {
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
fn eval_where(
    expr: &Expr,
    columns: &[String],
    row: &[Value],
) -> Result<bool> {
    match expr {
        Expr::BinaryOp { left, op, right } => {
            use sqlparser::ast::BinaryOperator;
            match op {
                BinaryOperator::And => {
                    Ok(eval_where(left, columns, row)?
                        && eval_where(right, columns, row)?)
                }
                BinaryOperator::Or => {
                    Ok(eval_where(left, columns, row)?
                        || eval_where(right, columns, row)?)
                }
                BinaryOperator::Eq => {
                    let l = eval_expr(left, columns, row)?;
                    let r = eval_expr(right, columns, row)?;
                    Ok(values_eq(&l, &r))
                }
                BinaryOperator::NotEq => {
                    let l = eval_expr(left, columns, row)?;
                    let r = eval_expr(right, columns, row)?;
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
fn eval_expr(
    expr: &Expr,
    columns: &[String],
    row: &[Value],
) -> Result<Value> {
    match expr {
        Expr::Identifier(ident) => {
            let name = ident.value.to_uppercase();
            let idx = columns
                .iter()
                .position(|c| c == &name)
                .ok_or_else(|| sql_error(SqlState::ColumnNotFound, format!("column {name} not found")))?;
            Ok(row[idx].clone())
        }
        Expr::CompoundIdentifier(parts) => {
            let name = parts
                .last()
                .map(|i| i.value.to_uppercase())
                .ok_or_else(|| sql_error(SqlState::SyntaxError, "empty identifier"))?;
            let idx = columns
                .iter()
                .position(|c| c == &name)
                .ok_or_else(|| sql_error(SqlState::ColumnNotFound, format!("column {name} not found")))?;
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
    use crate::sql::parser;

    fn test_catalog() -> Catalog {
        use crate::catalog::types::*;
        Catalog {
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
        }
    }

    #[test]
    fn select_star_from_systablespaces() {
        let catalog = test_catalog();
        let stmts = parser::parse("SELECT * FROM SYSTABLESPACES").unwrap();
        let rs = execute(&stmts[0], &catalog).unwrap();
        assert_eq!(rs.columns.len(), 7);
        assert_eq!(rs.rows.len(), 1);
    }

    #[test]
    fn select_specific_columns() {
        let catalog = test_catalog();
        let stmts = parser::parse("SELECT tbspace, tbspaceid FROM SYSTABLESPACES").unwrap();
        let rs = execute(&stmts[0], &catalog).unwrap();
        assert_eq!(rs.columns, vec!["TBSPACE", "TBSPACEID"]);
        assert_eq!(rs.rows.len(), 1);
    }

    #[test]
    fn select_with_schema_prefix() {
        let catalog = test_catalog();
        let stmts =
            parser::parse("SELECT * FROM RQSYS.SYSTABLESPACES").unwrap();
        let rs = execute(&stmts[0], &catalog).unwrap();
        assert_eq!(rs.rows.len(), 1);
    }

    #[test]
    fn select_with_where_eq() {
        let catalog = test_catalog();
        let stmts = parser::parse(
            "SELECT tbspace FROM SYSTABLESPACES WHERE tbspaceid = 1",
        )
        .unwrap();
        let rs = execute(&stmts[0], &catalog).unwrap();
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0].to_string(), "SYSTBSP");
    }

    #[test]
    fn select_with_where_no_match() {
        let catalog = test_catalog();
        let stmts = parser::parse(
            "SELECT tbspace FROM SYSTABLESPACES WHERE tbspaceid = 99",
        )
        .unwrap();
        let rs = execute(&stmts[0], &catalog).unwrap();
        assert_eq!(rs.rows.len(), 0);
    }

    #[test]
    fn select_with_string_where() {
        let catalog = test_catalog();
        let stmts = parser::parse(
            "SELECT * FROM SYSCOLUMNS WHERE tabname = 'SYSTABLESPACES'",
        )
        .unwrap();
        let rs = execute(&stmts[0], &catalog).unwrap();
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
        let catalog = test_catalog();
        let stmts = parser::parse("SELECT * FROM NONEXISTENT").unwrap();
        assert_sqlstate(execute(&stmts[0], &catalog), SqlState::TableNotFound);
    }

    #[test]
    fn error_column_not_found() {
        let catalog = test_catalog();
        let stmts =
            parser::parse("SELECT bogus FROM SYSTABLESPACES").unwrap();
        assert_sqlstate(execute(&stmts[0], &catalog), SqlState::ColumnNotFound);
    }

    #[test]
    fn error_column_not_found_in_where() {
        let catalog = test_catalog();
        let stmts = parser::parse(
            "SELECT * FROM SYSTABLESPACES WHERE bogus = 1",
        )
        .unwrap();
        assert_sqlstate(execute(&stmts[0], &catalog), SqlState::ColumnNotFound);
    }

    #[test]
    fn error_unsupported_insert() {
        let catalog = test_catalog();
        let stmts = parser::parse(
            "INSERT INTO SYSTABLESPACES VALUES (4, 'X', 'D', 4096, 'N')",
        )
        .unwrap();
        assert_sqlstate(
            execute(&stmts[0], &catalog),
            SqlState::FeatureNotSupported,
        );
    }

    #[test]
    fn error_unsupported_delete() {
        let catalog = test_catalog();
        let stmts =
            parser::parse("DELETE FROM SYSTABLESPACES WHERE tbspaceid = 1").unwrap();
        assert_sqlstate(
            execute(&stmts[0], &catalog),
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
