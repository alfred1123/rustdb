use std::collections::HashMap;

use sqlparser::ast::{
    ColumnOption, Expr, ObjectType, SelectItem, SetExpr, Statement, TableFactor,
};

use crate::catalog::cache::CatalogCache;
use crate::catalog::row::{RowReader, RowWriter, LENGTH_PREFIX_SIZE, MIN_COLUMN_BYTES};
use crate::catalog::types::{Column, Schema, Table, MIN_CHAR_LENGTH, MAX_CHAR_LENGTH};
use crate::error::{sql_error, Result, SqlState};
use crate::sql::types::{ResultSet, TableRef, Value};
use crate::storage::heap::Rid;
use crate::storage::page::PAGE_HEADER_SIZE;
use crate::storage::tablespace::TablespaceManager;
use crate::storage::tuple::TUPLE_HEADER_SIZE;

/// Execute a parsed SQL statement against the storage engine.
pub fn execute(
    stmt: &Statement,
    cache: &mut CatalogCache,
    tsm: &mut TablespaceManager,
) -> Result<ResultSet> {
    match stmt {
        Statement::Query(query) => execute_query(query, cache, tsm),
        Statement::Insert(insert) => execute_insert(insert, cache, tsm),
        Statement::Delete(delete) => execute_delete(delete, cache, tsm),
        Statement::Update {
            table,
            assignments,
            selection,
            ..
        } => execute_update(table, assignments, selection.as_ref(), cache, tsm),
        Statement::CreateTable(create_table) => {
            execute_create_table(create_table, cache, tsm)
        }
        Statement::Drop {
            object_type: ObjectType::Table,
            names,
            if_exists,
            ..
        } => execute_drop_table(names, *if_exists, cache, tsm),
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

    let table_ref = resolve_table_factor(&from.relation, cache)?;
    let table_ref = resolve_with_search_path(table_ref, cache);

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

    let (column_names, column_index) = cache
        .get_column_meta(&table_ref.schema, &table_ref.table)
        .ok_or_else(|| {
            sql_error(
                SqlState::TableNotFound,
                format!("table {}.{} not found", table_ref.schema, table_ref.table),
            )
        })?;

    // Scan rows from storage via tablespace manager.
    let raw_rows = tsm.table_scan(&table_ref.schema, &table_ref.table)?;
    let all_rows: Vec<Vec<Value>> = raw_rows
        .iter()
        .map(|(_, bytes)| deserialize_row(bytes, columns))
        .collect::<Result<_>>()?;

    // Resolve SELECT list using O(1) column index.
    let (selected_columns, selected_indices) =
        resolve_select_list(&select.projection, column_names, column_index)?;

    // Apply WHERE filter.
    let filtered_rows = match &select.selection {
        Some(expr) => {
            let mut result = Vec::new();
            for row in &all_rows {
                if eval_where(expr, column_index, row)? {
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
        sqlparser::ast::TableObject::TableName(name) => resolve_table_name(name, cache)?,
        _ => return Err(sql_error(SqlState::FeatureNotSupported, "table functions not supported")),
    };
    let table_ref = resolve_with_search_path(table_ref, cache);
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
        let (_, column_index) = cache
            .get_column_meta(&table_ref.schema, &table_ref.table)
            .ok_or_else(|| {
                sql_error(
                    SqlState::TableNotFound,
                    format!("table {}.{} not found", table_ref.schema, table_ref.table),
                )
            })?;
        insert.columns
            .iter()
            .map(|ident| {
                let name = ident.value.to_uppercase();
                column_index.get(&name).copied().ok_or_else(|| {
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
    let table_ref = resolve_table_factor(&from_tables[0].relation, cache)?;
    let table_ref = resolve_with_search_path(table_ref, cache);
    log::debug!("DELETE from {}.{}", table_ref.schema, table_ref.table);

    let columns = cache
        .get_columns(&table_ref.schema, &table_ref.table)
        .ok_or_else(|| {
            sql_error(
                SqlState::TableNotFound,
                format!("table {}.{} not found", table_ref.schema, table_ref.table),
            )
        })?;

    let (_, column_index) = cache
        .get_column_meta(&table_ref.schema, &table_ref.table)
        .ok_or_else(|| {
            sql_error(
                SqlState::TableNotFound,
                format!("table {}.{} not found", table_ref.schema, table_ref.table),
            )
        })?;

    // Scan all rows, evaluate WHERE, collect RIDs to delete.
    let raw_rows = tsm.table_scan(&table_ref.schema, &table_ref.table)?;
    let mut rids_to_delete = Vec::new();
    for (rid, bytes) in &raw_rows {
        let row = deserialize_row(bytes, columns)?;
        let matches = match &delete.selection {
            Some(expr) => eval_where(expr, column_index, &row)?,
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

// ── UPDATE ──

fn execute_update(
    table: &sqlparser::ast::TableWithJoins,
    assignments: &[sqlparser::ast::Assignment],
    selection: Option<&Expr>,
    cache: &CatalogCache,
    tsm: &mut TablespaceManager,
) -> Result<ResultSet> {
    let table_ref = resolve_table_factor(&table.relation, cache)?;
    let table_ref = resolve_with_search_path(table_ref, cache);
    log::debug!("UPDATE {}.{}", table_ref.schema, table_ref.table);

    let columns = cache
        .get_columns(&table_ref.schema, &table_ref.table)
        .ok_or_else(|| {
            sql_error(
                SqlState::TableNotFound,
                format!("table {}.{} not found", table_ref.schema, table_ref.table),
            )
        })?;

    let (_, column_index) = cache
        .get_column_meta(&table_ref.schema, &table_ref.table)
        .ok_or_else(|| {
            sql_error(
                SqlState::TableNotFound,
                format!("table {}.{} not found", table_ref.schema, table_ref.table),
            )
        })?;

    // Resolve SET assignments to (column_index, expression) pairs.
    let mut set_pairs: Vec<(usize, &Expr)> = Vec::with_capacity(assignments.len());
    for assign in assignments {
        let col_name = match &assign.target {
            sqlparser::ast::AssignmentTarget::ColumnName(name) => {
                name.0.last()
                    .map(|i| i.as_ident().unwrap().value.to_uppercase())
                    .ok_or_else(|| sql_error(SqlState::SyntaxError, "empty column name in SET"))?
            }
            sqlparser::ast::AssignmentTarget::Tuple(_) => {
                return Err(sql_error(
                    SqlState::FeatureNotSupported,
                    "tuple assignment in UPDATE is not supported",
                ));
            }
        };
        let idx = *column_index.get(&col_name).ok_or_else(|| {
            sql_error(SqlState::ColumnNotFound, format!("column {col_name} not found"))
        })?;
        set_pairs.push((idx, &assign.value));
    }

    // Scan all rows, find matches, apply updates via delete+insert.
    let raw_rows = tsm.table_scan(&table_ref.schema, &table_ref.table)?;
    let mut updates: Vec<(Rid, Vec<u8>)> = Vec::new();

    for (rid, bytes) in &raw_rows {
        let row = deserialize_row(bytes, columns)?;
        let matches = match selection {
            Some(expr) => eval_where(expr, column_index, &row)?,
            None => true,
        };
        if matches {
            // Apply SET assignments: evaluate each expression against the
            // current row so that `SET col = col + 1` style works (though
            // arithmetic isn't wired up yet, literal values work now).
            let mut new_row = row;
            for &(col_idx, value_expr) in &set_pairs {
                new_row[col_idx] = eval_expr(value_expr, column_index, &new_row)?;
            }
            let new_bytes = serialize_row(&new_row, columns)?;
            updates.push((*rid, new_bytes));
        }
    }

    let mut updated = 0i32;
    for (rid, new_bytes) in updates {
        tsm.update_row(&table_ref.schema, &table_ref.table, rid, &new_bytes)?;
        updated += 1;
    }

    Ok(ResultSet {
        columns: vec!["ROWS_UPDATED".into()],
        rows: vec![vec![Value::Integer(updated)]],
    })
}

// ── CREATE TABLE ──

fn execute_create_table(
    create: &sqlparser::ast::CreateTable,
    cache: &mut CatalogCache,
    tsm: &mut TablespaceManager,
) -> Result<ResultSet> {
    let table_ref = resolve_table_name(&create.name, cache)?;
    let schema = &table_ref.schema;
    let table_name = &table_ref.table;
    log::debug!("CREATE TABLE {schema}.{table_name}");

    // Reject creating tables in the system catalog schema (only when
    // explicitly schema-qualified, e.g., CREATE TABLE RQSYS.foo).
    let explicit_schema = create.name.0.len() > 1;
    if explicit_schema && *schema == cache.config().sys_schema {
        return Err(sql_error(
            SqlState::SystemSchemaViolation,
            format!("cannot create user table in system schema {}", cache.config().sys_schema),
        ));
    }

    // Reject if table already exists.
    if cache.get_table(schema, table_name).is_some() {
        return Err(sql_error(
            SqlState::TableAlreadyExists,
            format!("table {schema}.{table_name} already exists"),
        ));
    }

    if create.columns.is_empty() {
        return Err(sql_error(
            SqlState::SyntaxError,
            "CREATE TABLE requires at least one column",
        ));
    }

    // Reject if too many columns (dynamic limit from page size).
    // Usable payload per row = page − page header − 1 slot entry − MVCC tuple header.
    let tbspaceid = cache.default_tablespace_id();
    let pagesize = cache
        .get_tablespace_by_id(tbspaceid as i32)
        .map(|ts| ts.pagesize as usize)
        .unwrap_or(4096);
    let max_payload = pagesize - PAGE_HEADER_SIZE - 4 - TUPLE_HEADER_SIZE;
    let max_columns = max_payload / MIN_COLUMN_BYTES;
    if create.columns.len() > max_columns {
        return Err(sql_error(
            SqlState::TooManyColumns,
            format!(
                "table exceeds maximum column count for {pagesize}-byte pages \
                 ({} > {max_columns})",
                create.columns.len(),
            ),
        ));
    }

    // Map sqlparser column definitions to our catalog Column structs.
    let mut seen_names = std::collections::HashSet::new();
    let mut columns: Vec<Column> = Vec::with_capacity(create.columns.len());
    for (ordinal, col_def) in create.columns.iter().enumerate() {
        let col_name = col_def.name.value.to_uppercase();
        if !seen_names.insert(col_name.clone()) {
            return Err(sql_error(
                SqlState::DuplicateColumnName,
                format!("duplicate column name: {col_name}"),
            ));
        }
        let type_name = map_data_type(&col_def.data_type)?;
        let nullable = !col_def.options.iter().any(|o| matches!(o.option, ColumnOption::NotNull));
        columns.push(Column {
            name: col_name,
            tabname: table_name.clone(),
            schemaname: schema.clone(),
            ordinal: ordinal as i16,
            typename: type_name,
            nullable,
        });
    }

    let colcount = columns.len() as i16;

    // Validate that the maximum possible row size fits on a page.
    let max_row = max_row_size(&columns);
    if max_row > max_payload {
        return Err(sql_error(
            SqlState::RowTooLarge,
            format!(
                "maximum row size {max_row} bytes exceeds page limit \
                 {max_payload} bytes (page size {pagesize})",
            ),
        ));
    }

    // 1. If the schema doesn't exist yet, register it in SYSSCHEMAS.
    if !cache.has_schema(schema) {
        let mut w = RowWriter::new();
        w.write_string(schema);
        tsm.insert_row(&cache.config().sys_schema, "SYSSCHEMAS", &w.finish())?;
        cache.register_schema(Schema { name: schema.clone() });
    }

    // 2. Insert a row into SYSTABLES.
    let tableid = cache.next_table_id();
    let mut w = RowWriter::new();
    w.write_i32(tableid);
    w.write_string(table_name);
    w.write_string(schema);
    w.write_i16(tbspaceid);
    w.write_i16(colcount);
    tsm.insert_row(&cache.config().sys_schema, "SYSTABLES", &w.finish())?;

    // 3. Insert one row per column into SYSCOLUMNS.
    for col in &columns {
        let mut w = RowWriter::new();
        w.write_string(&col.name);
        w.write_string(&col.tabname);
        w.write_string(&col.schemaname);
        w.write_i16(col.ordinal);
        w.write_string(&col.typename);
        w.write_bool(col.nullable);
        tsm.insert_row(&cache.config().sys_schema, "SYSCOLUMNS", &w.finish())?;
    }

    // 4. Create the empty heap file and register in TSM.
    tsm.register_new_table(schema, table_name, tbspaceid as i32)?;

    // 5. Register in CatalogCache so subsequent queries see the table.
    let table = Table {
        tableid,
        name: table_name.clone(),
        schemaname: schema.clone(),
        tbspaceid,
        colcount,
    };
    cache.register_table(table, columns);

    Ok(ResultSet {
        columns: vec!["STATUS".into()],
        rows: vec![vec![Value::Str(format!("TABLE {schema}.{table_name} CREATED"))]],
    })
}

// ── DROP TABLE ──

fn execute_drop_table(
    names: &[sqlparser::ast::ObjectName],
    if_exists: bool,
    cache: &mut CatalogCache,
    tsm: &mut TablespaceManager,
) -> Result<ResultSet> {
    if names.len() != 1 {
        return Err(sql_error(
            SqlState::FeatureNotSupported,
            "DROP TABLE supports exactly one table at a time",
        ));
    }

    let table_ref = resolve_table_name(&names[0], cache)?;
    let table_ref = resolve_with_search_path(table_ref, cache);
    let schema = &table_ref.schema;
    let table_name = &table_ref.table;
    log::debug!("DROP TABLE {schema}.{table_name}");

    let sys_schema = cache.config().sys_schema.clone();

    // Reject dropping system catalog tables.
    if *schema == sys_schema {
        return Err(sql_error(
            SqlState::SystemSchemaViolation,
            format!("cannot drop system table {schema}.{table_name}"),
        ));
    }

    // Check existence.
    if cache.get_table(schema, table_name).is_none() {
        if if_exists {
            return Ok(ResultSet {
                columns: vec!["STATUS".into()],
                rows: vec![vec![Value::Str(format!(
                    "TABLE {schema}.{table_name} DOES NOT EXIST (skipped)"
                ))]],
            });
        }
        return Err(sql_error(
            SqlState::TableNotFound,
            format!("table {schema}.{table_name} not found"),
        ));
    }

    // 1. Delete the matching row from SYSTABLES.
    delete_catalog_rows_for_table(
        &sys_schema, "SYSTABLES", schema, table_name, cache, tsm,
    )?;

    // 2. Delete matching rows from SYSCOLUMNS.
    delete_catalog_rows_for_table(
        &sys_schema, "SYSCOLUMNS", schema, table_name, cache, tsm,
    )?;

    // 3. Drop the heap file and FSM from disk + evict from buffer pool.
    tsm.drop_table(schema, table_name)?;

    // 4. Unregister from the in-memory catalog cache.
    cache.unregister_table(schema, table_name);

    Ok(ResultSet {
        columns: vec!["STATUS".into()],
        rows: vec![vec![Value::Str(format!("TABLE {schema}.{table_name} DROPPED"))]],
    })
}

/// Delete rows in a system catalog table that reference the given
/// `(schema, table)`. For SYSTABLES, matches on SCHEMANAME + NAME columns.
/// For SYSCOLUMNS, matches on SCHEMANAME + TABNAME columns.
fn delete_catalog_rows_for_table(
    sys_schema: &str,
    catalog_table: &str,
    target_schema: &str,
    target_table: &str,
    cache: &CatalogCache,
    tsm: &mut TablespaceManager,
) -> Result<()> {
    let columns = cache
        .get_columns(sys_schema, catalog_table)
        .ok_or_else(|| {
            sql_error(
                SqlState::TableNotFound,
                format!("catalog table {sys_schema}.{catalog_table} not found"),
            )
        })?;

    let (_, column_index) = cache
        .get_column_meta(sys_schema, catalog_table)
        .ok_or_else(|| {
            sql_error(
                SqlState::TableNotFound,
                format!("catalog table {sys_schema}.{catalog_table} not found"),
            )
        })?;

    let schema_col = if catalog_table == "SYSTABLES" { "SCHEMANAME" } else { "SCHEMANAME" };
    let name_col = if catalog_table == "SYSTABLES" { "NAME" } else { "TABNAME" };

    let schema_idx = *column_index.get(schema_col).ok_or_else(|| {
        sql_error(SqlState::ColumnNotFound, format!("column {schema_col} not found in {catalog_table}"))
    })?;
    let name_idx = *column_index.get(name_col).ok_or_else(|| {
        sql_error(SqlState::ColumnNotFound, format!("column {name_col} not found in {catalog_table}"))
    })?;

    let raw_rows = tsm.table_scan(sys_schema, catalog_table)?;
    let mut rids_to_delete = Vec::new();

    for (rid, bytes) in &raw_rows {
        let row = deserialize_row(bytes, columns)?;
        let row_schema = row[schema_idx].to_string();
        let row_name = row[name_idx].to_string();
        if row_schema == target_schema && row_name == target_table {
            rids_to_delete.push(*rid);
        }
    }

    for rid in rids_to_delete {
        tsm.delete_row(sys_schema, catalog_table, rid)?;
    }

    Ok(())
}

/// Compute the maximum serialized row size (in bytes) for a set of columns.
///
/// Each field is serialized as: 8-byte length prefix (u64 LE) + value bytes.
/// For variable-length types (VARCHAR), the declared maximum length is used.
fn max_row_size(columns: &[Column]) -> usize {
    columns.iter().map(|col| {
        let base = col.typename.split('(').next().unwrap_or(&col.typename);
        let data_size = match base {
            "SMALLINT" => 2,
            "INTEGER" => 4,
            "BIGINT" | "DOUBLE" => 8,
            "TIMESTAMP" => 33, // "YYYY-MM-DD HH:MM:SS.nnnnnnnnn UTC"
            "CHAR" | "VARCHAR" => {
                // Extract length from "CHAR(n)" / "VARCHAR(n)".
                col.typename
                    .split('(')
                    .nth(1)
                    .and_then(|s| s.trim_end_matches(')').parse::<usize>().ok())
                    .unwrap_or(1)
            }
            _ => 255, // conservative default for unknown types
        };
        LENGTH_PREFIX_SIZE + data_size
    }).sum()
}

/// Validate that a CHAR/VARCHAR length is within bounds.
fn validate_char_length(length: u64, type_name: &str) -> Result<()> {
    if length < MIN_CHAR_LENGTH || length > MAX_CHAR_LENGTH {
        return Err(sql_error(
            SqlState::InvalidColumnLength,
            format!(
                "invalid length {length} for {type_name} \
                 (must be {MIN_CHAR_LENGTH}..{MAX_CHAR_LENGTH})",
            ),
        ));
    }
    Ok(())
}

/// Map a sqlparser DataType to our catalog type name string.
fn map_data_type(dt: &sqlparser::ast::DataType) -> Result<String> {
    use sqlparser::ast::{CharacterLength, DataType};
    match dt {
        DataType::SmallInt(_) => Ok("SMALLINT".into()),
        DataType::Int(_) | DataType::Integer(_) => Ok("INTEGER".into()),
        DataType::BigInt(_) => Ok("BIGINT".into()),
        DataType::Double(_) | DataType::DoublePrecision => Ok("DOUBLE".into()),
        DataType::Varchar(len_opt) => {
            match len_opt {
                Some(CharacterLength::IntegerLength { length, .. }) => {
                    validate_char_length(*length, "VARCHAR")?;
                    Ok(format!("VARCHAR({length})"))
                }
                _ => Ok("VARCHAR(255)".into()), // default length
            }
        }
        DataType::Char(len_opt) | DataType::Character(len_opt) => {
            match len_opt {
                Some(CharacterLength::IntegerLength { length, .. }) => {
                    validate_char_length(*length, "CHAR")?;
                    Ok(format!("CHAR({length})"))
                }
                _ => Ok("CHAR(1)".into()), // default length
            }
        }
        DataType::Timestamp(_, _) => Ok("TIMESTAMP".into()),
        DataType::Boolean => Ok("CHAR(1)".into()),
        other => Err(sql_error(
            SqlState::FeatureNotSupported,
            format!("unsupported data type: {other}"),
        )),
    }
}

// ── Table reference helpers ──

/// Apply the schema search path: if the table isn't found in the resolved
/// schema and the schema is the configured default, try the default schema
/// then the system schema.  Returns the original ref if nothing matches
/// (so the caller produces the usual "table not found").
fn resolve_with_search_path(table_ref: TableRef, cache: &CatalogCache) -> TableRef {
    // Exact match — no search needed.
    if cache.get_table(&table_ref.schema, &table_ref.table).is_some() {
        return table_ref;
    }
    // Only search when the user didn't explicitly qualify.
    let default_schema = &cache.config().default_schema;
    if table_ref.schema == *default_schema {
        for sch in [default_schema.as_str(), cache.config().sys_schema.as_str()] {
            if cache.get_table(sch, &table_ref.table).is_some() {
                return TableRef {
                    schema: sch.to_string(),
                    table: table_ref.table,
                };
            }
        }
    }
    table_ref
}

fn resolve_table_factor(relation: &TableFactor, cache: &CatalogCache) -> Result<TableRef> {
    match relation {
        TableFactor::Table { name, .. } => resolve_table_name(name, cache),
        _ => Err(sql_error(SqlState::FeatureNotSupported, "unsupported FROM clause")),
    }
}

fn resolve_table_name(name: &sqlparser::ast::ObjectName, cache: &CatalogCache) -> Result<TableRef> {
    let default_schema = &cache.config().default_schema;
    let parts: Vec<&str> = name
        .0
        .iter()
        .map(|i| i.as_ident().unwrap().value.as_str())
        .collect();
    match parts.len() {
        1 => TableRef::resolve(None, parts[0], default_schema),
        2 => TableRef::resolve(Some(parts[0]), parts[1], default_schema),
        _ => Err(sql_error(
            SqlState::SyntaxError,
            format!("invalid table reference: {name}"),
        )),
    }
}

// ── Generic row (de)serialization ──

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
            let path = std::env::temp_dir().join(format!("rqdb_exec_{name}"));
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
            crate::catalog::loader::load_catalog(&dir.0, &cfg).unwrap();
        let cache = CatalogCache::new(catalog, cfg);
        let tsm = TablespaceManager::open(&dir.0, &cache).unwrap();
        (cache, tsm, dir)
    }

    #[test]
    fn select_star_from_systablespaces() {
        let (mut cache, mut tsm, _dir) = test_fixture("sel_star");
        let stmts = parser::parse("SELECT * FROM SYSTABLESPACES").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.columns.len(), 7);
        assert_eq!(rs.rows.len(), 3); // 3 tablespaces bootstrapped
    }

    #[test]
    fn select_specific_columns() {
        let (mut cache, mut tsm, _dir) = test_fixture("sel_cols");
        let stmts = parser::parse("SELECT tbspace, tbspaceid FROM SYSTABLESPACES").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.columns, vec!["TBSPACE", "TBSPACEID"]);
        assert_eq!(rs.rows.len(), 3);
    }

    #[test]
    fn select_with_schema_prefix() {
        let (mut cache, mut tsm, _dir) = test_fixture("sel_schema");
        let stmts =
            parser::parse("SELECT * FROM RQSYS.SYSTABLESPACES").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 3);
    }

    #[test]
    fn select_with_where_eq() {
        let (mut cache, mut tsm, _dir) = test_fixture("sel_where");
        let stmts = parser::parse(
            "SELECT tbspace FROM SYSTABLESPACES WHERE tbspaceid = 1",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0].to_string(), "SYSTBSP");
    }

    #[test]
    fn select_with_where_no_match() {
        let (mut cache, mut tsm, _dir) = test_fixture("sel_nomatch");
        let stmts = parser::parse(
            "SELECT tbspace FROM SYSTABLESPACES WHERE tbspaceid = 99",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 0);
    }

    #[test]
    fn select_with_string_where() {
        let (mut cache, mut tsm, _dir) = test_fixture("sel_str");
        let stmts = parser::parse(
            "SELECT * FROM SYSCOLUMNS WHERE tabname = 'SYSTABLESPACES'",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 7); // 7 columns in SYSTABLESPACES
    }

    #[test]
    fn select_all_catalog_tables() {
        let (mut cache, mut tsm, _dir) = test_fixture("sel_all");

        // SYSTABLES: 5 tables
        let stmts = parser::parse("SELECT * FROM SYSTABLES").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 5);

        // SYSSCHEMAS: 2 schemas (RQSYS + PUBLIC)
        let stmts = parser::parse("SELECT * FROM SYSSCHEMAS").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 2);

        // SYSBUFFERPOOLS: 4 pools
        let stmts = parser::parse("SELECT * FROM SYSBUFFERPOOLS").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 4);
    }

    // ── INSERT tests ──

    #[test]
    fn insert_and_select() {
        let (mut cache, mut tsm, _dir) = test_fixture("ins_sel");

        // Insert a new tablespace row.
        let stmts = parser::parse(
            "INSERT INTO SYSTABLESPACES VALUES (10, 'NEWTBSP', 'D', 'A', 4096, 'N', 1)",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "1"); // 1 row inserted

        // Verify it's there via SELECT.
        let stmts = parser::parse(
            "SELECT tbspace FROM SYSTABLESPACES WHERE tbspaceid = 10",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0].to_string(), "NEWTBSP");
    }

    #[test]
    fn insert_with_column_list() {
        let (mut cache, mut tsm, _dir) = test_fixture("ins_cols");

        let stmts = parser::parse(
            "INSERT INTO SYSSCHEMAS (NAME) VALUES ('USERSCH')",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "1");

        let stmts = parser::parse("SELECT * FROM SYSSCHEMAS").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 3); // RQSYS + PUBLIC + USERSCH
    }

    #[test]
    fn insert_multiple_rows() {
        let (mut cache, mut tsm, _dir) = test_fixture("ins_multi");

        let stmts = parser::parse(
            "INSERT INTO SYSSCHEMAS VALUES ('S1'), ('S2'), ('S3')",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "3");

        let stmts = parser::parse("SELECT * FROM SYSSCHEMAS").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 5); // RQSYS + PUBLIC + S1 + S2 + S3
    }

    // ── DELETE tests ──

    #[test]
    fn delete_with_where() {
        let (mut cache, mut tsm, _dir) = test_fixture("del_where");

        // 3 tablespaces exist. Delete TEMPTBSP (id=3).
        let stmts = parser::parse(
            "DELETE FROM SYSTABLESPACES WHERE tbspaceid = 3",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "1");

        let stmts = parser::parse("SELECT * FROM SYSTABLESPACES").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 2); // SYSTBSP + USERTBSP remain
    }

    #[test]
    fn delete_all() {
        let (mut cache, mut tsm, _dir) = test_fixture("del_all");

        let stmts = parser::parse("DELETE FROM SYSSCHEMAS").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "2"); // RQSYS + PUBLIC deleted

        let stmts = parser::parse("SELECT * FROM SYSSCHEMAS").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 0);
    }

    #[test]
    fn delete_no_match() {
        let (mut cache, mut tsm, _dir) = test_fixture("del_nomatch");

        let stmts = parser::parse(
            "DELETE FROM SYSTABLESPACES WHERE tbspaceid = 99",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "0");
    }

    #[test]
    fn update_with_where() {
        let (mut cache, mut tsm, _dir) = test_fixture("upd_where");

        // SYSTBSP has tbspaceid=1. Update its tbspace name.
        let stmts = parser::parse(
            "UPDATE SYSTABLESPACES SET tbspace = 'RENAMED' WHERE tbspaceid = 1",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "1"); // 1 row updated

        // Verify it was actually changed.
        let stmts = parser::parse(
            "SELECT tbspace FROM SYSTABLESPACES WHERE tbspaceid = 1",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0].to_string(), "RENAMED");
    }

    #[test]
    fn update_all_rows() {
        let (mut cache, mut tsm, _dir) = test_fixture("upd_all");

        // Update all tablespace states to 'Y'.
        let stmts = parser::parse(
            "UPDATE SYSTABLESPACES SET state = 'Y'",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "3"); // 3 rows updated

        // Verify all rows have state = 'Y'.
        let stmts = parser::parse(
            "SELECT state FROM SYSTABLESPACES WHERE state = 'Y'",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 3);
    }

    #[test]
    fn update_no_match() {
        let (mut cache, mut tsm, _dir) = test_fixture("upd_nomatch");

        let stmts = parser::parse(
            "UPDATE SYSTABLESPACES SET tbspace = 'X' WHERE tbspaceid = 99",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "0");
    }

    #[test]
    fn update_multiple_columns() {
        let (mut cache, mut tsm, _dir) = test_fixture("upd_multi_col");

        let stmts = parser::parse(
            "UPDATE SYSTABLESPACES SET tbspace = 'NEW', tbspacetype = 'S' WHERE tbspaceid = 2",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "1");

        let stmts = parser::parse(
            "SELECT tbspace, tbspacetype FROM SYSTABLESPACES WHERE tbspaceid = 2",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0].to_string(), "NEW");
        assert_eq!(rs.rows[0][1].to_string(), "S");
    }

    #[test]
    fn update_preserves_unmodified_rows() {
        let (mut cache, mut tsm, _dir) = test_fixture("upd_preserve");

        // Update only tbspaceid=1, verify others unchanged.
        let stmts = parser::parse(
            "UPDATE SYSTABLESPACES SET tbspace = 'CHANGED' WHERE tbspaceid = 1",
        )
        .unwrap();
        execute(&stmts[0], &mut cache, &mut tsm).unwrap();

        // tbspaceid=2 should be unchanged.
        let stmts = parser::parse(
            "SELECT tbspace FROM SYSTABLESPACES WHERE tbspaceid = 2",
        )
        .unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0].to_string(), "USERTBSP");
    }

    #[test]
    fn update_column_not_found() {
        let (mut cache, mut tsm, _dir) = test_fixture("upd_bad_col");
        let stmts = parser::parse(
            "UPDATE SYSTABLESPACES SET bogus = 'X' WHERE tbspaceid = 1",
        )
        .unwrap();
        assert_sqlstate(
            execute(&stmts[0], &mut cache, &mut tsm),
            SqlState::ColumnNotFound,
        );
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
        let (mut cache, mut tsm, _dir) = test_fixture("err_table");
        let stmts = parser::parse("SELECT * FROM NONEXISTENT").unwrap();
        assert_sqlstate(execute(&stmts[0], &mut cache, &mut tsm), SqlState::TableNotFound);
    }

    #[test]
    fn error_column_not_found() {
        let (mut cache, mut tsm, _dir) = test_fixture("err_col");
        let stmts =
            parser::parse("SELECT bogus FROM SYSTABLESPACES").unwrap();
        assert_sqlstate(execute(&stmts[0], &mut cache, &mut tsm), SqlState::ColumnNotFound);
    }

    #[test]
    fn error_column_not_found_in_where() {
        let (mut cache, mut tsm, _dir) = test_fixture("err_col_where");
        let stmts = parser::parse(
            "SELECT * FROM SYSTABLESPACES WHERE bogus = 1",
        )
        .unwrap();
        assert_sqlstate(execute(&stmts[0], &mut cache, &mut tsm), SqlState::ColumnNotFound);
    }

    // ── CREATE TABLE tests ──

    #[test]
    fn create_table_basic() {
        let (mut cache, mut tsm, _dir) = test_fixture("ct_basic");
        let stmts = parser::parse(
            "CREATE TABLE employees (id INTEGER NOT NULL, name VARCHAR(50), active CHAR(1))",
        ).unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert!(rs.rows[0][0].to_string().contains("CREATED"));

        // Table should be visible in SYSTABLES.
        let stmts = parser::parse(
            "SELECT name, colcount FROM SYSTABLES WHERE name = 'EMPLOYEES'",
        ).unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0].to_string(), "EMPLOYEES");
        assert_eq!(rs.rows[0][1].to_string(), "3");
    }

    #[test]
    fn create_table_insert_and_select() {
        let (mut cache, mut tsm, _dir) = test_fixture("ct_ins_sel");
        let stmts = parser::parse(
            "CREATE TABLE items (id INTEGER NOT NULL, label VARCHAR(30))",
        ).unwrap();
        execute(&stmts[0], &mut cache, &mut tsm).unwrap();

        // INSERT into the new table.
        let stmts = parser::parse(
            "INSERT INTO items VALUES (1, 'Widget')",
        ).unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "1");

        // SELECT from the new table.
        let stmts = parser::parse("SELECT * FROM items").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.columns, vec!["ID", "LABEL"]);
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0].to_string(), "1");
        assert_eq!(rs.rows[0][1].to_string(), "Widget");
    }

    #[test]
    fn create_table_with_schema() {
        let (mut cache, mut tsm, _dir) = test_fixture("ct_schema");
        let stmts = parser::parse(
            "CREATE TABLE myapp.users (uid INTEGER NOT NULL, email VARCHAR(100))",
        ).unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert!(rs.rows[0][0].to_string().contains("CREATED"));

        // New schema should appear in SYSSCHEMAS.
        let stmts = parser::parse(
            "SELECT name FROM SYSSCHEMAS WHERE name = 'MYAPP'",
        ).unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 1);

        // INSERT and SELECT through the schema-qualified name.
        let stmts = parser::parse(
            "INSERT INTO myapp.users VALUES (42, 'alice@example.com')",
        ).unwrap();
        execute(&stmts[0], &mut cache, &mut tsm).unwrap();

        let stmts = parser::parse("SELECT * FROM myapp.users").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0].to_string(), "42");
    }

    #[test]
    fn create_table_duplicate_rejected() {
        let (mut cache, mut tsm, _dir) = test_fixture("ct_dup");
        let stmts = parser::parse(
            "CREATE TABLE dup_test (x INTEGER)",
        ).unwrap();
        execute(&stmts[0], &mut cache, &mut tsm).unwrap();

        // Second CREATE TABLE with same name should fail.
        let stmts = parser::parse(
            "CREATE TABLE dup_test (y VARCHAR(10))",
        ).unwrap();
        assert_sqlstate(
            execute(&stmts[0], &mut cache, &mut tsm),
            SqlState::TableAlreadyExists,
        );
    }

    #[test]
    fn create_table_row_too_large() {
        let (mut cache, mut tsm, _dir) = test_fixture("ct_toobig");
        // Page size is 4096. Header=24, slot=4, tuple header=16 → max payload=4052.
        // VARCHAR(4000) → 8+4000=4008, plus an INTEGER → 8+4=12.
        // Total: 4020 bytes — fits. Add another VARCHAR(100) → 8+100=108
        // giving 4128 — exceeds 4052.
        let stmts = parser::parse(
            "CREATE TABLE toobig (id INTEGER, data VARCHAR(4000), extra VARCHAR(100))",
        ).unwrap();
        assert_sqlstate(
            execute(&stmts[0], &mut cache, &mut tsm),
            SqlState::RowTooLarge,
        );
    }

    #[test]
    fn create_table_row_just_fits() {
        let (mut cache, mut tsm, _dir) = test_fixture("ct_justfit");
        // max payload = 4052 (page 4096 − header 24 − slot 4 − tuple hdr 16).
        // INTEGER=12, VARCHAR(4032)=8+4032=4040 → 4052 exactly.
        let stmts = parser::parse(
            "CREATE TABLE justfit (id INTEGER, data VARCHAR(4032))",
        ).unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert!(rs.rows[0][0].to_string().contains("CREATED"));
    }

    #[test]
    fn create_table_duplicate_column_name() {
        let (mut cache, mut tsm, _dir) = test_fixture("ct_dup_col");
        let stmts = parser::parse(
            "CREATE TABLE bad (id INTEGER, name VARCHAR(10), id SMALLINT)",
        ).unwrap();
        assert_sqlstate(
            execute(&stmts[0], &mut cache, &mut tsm),
            SqlState::DuplicateColumnName,
        );
    }

    #[test]
    fn create_table_invalid_char_length_zero() {
        let (mut cache, mut tsm, _dir) = test_fixture("ct_len0");
        let stmts = parser::parse(
            "CREATE TABLE bad (name CHAR(0))",
        ).unwrap();
        assert_sqlstate(
            execute(&stmts[0], &mut cache, &mut tsm),
            SqlState::InvalidColumnLength,
        );
    }

    #[test]
    fn create_table_invalid_varchar_length_too_large() {
        let (mut cache, mut tsm, _dir) = test_fixture("ct_len_big");
        let stmts = parser::parse(
            "CREATE TABLE bad (data VARCHAR(40000))",
        ).unwrap();
        assert_sqlstate(
            execute(&stmts[0], &mut cache, &mut tsm),
            SqlState::InvalidColumnLength,
        );
    }

    #[test]
    fn create_table_too_many_columns() {
        let (mut cache, mut tsm, _dir) = test_fixture("ct_maxcol");
        // Page 4096: max_payload=4052 (4096−24−4−16), min 9 bytes/col → limit=450.
        // 451 CHAR(1) columns should exceed the dynamic limit.
        let cols: Vec<String> = (0..451).map(|i| format!("c{i} CHAR(1)")).collect();
        let sql = format!("CREATE TABLE huge ({})", cols.join(", "));
        let stmts = parser::parse(&sql).unwrap();
        assert_sqlstate(
            execute(&stmts[0], &mut cache, &mut tsm),
            SqlState::TooManyColumns,
        );
    }

    #[test]
    fn create_table_system_schema_rejected() {
        let (mut cache, mut tsm, _dir) = test_fixture("ct_rqsys");
        let stmts = parser::parse(
            "CREATE TABLE RQSYS.forbidden (id INTEGER)",
        ).unwrap();
        assert_sqlstate(
            execute(&stmts[0], &mut cache, &mut tsm),
            SqlState::SystemSchemaViolation,
        );
    }

    #[test]
    fn create_table_columns_in_syscolumns() {
        let (mut cache, mut tsm, _dir) = test_fixture("ct_cols");
        let stmts = parser::parse(
            "CREATE TABLE parts (partno SMALLINT NOT NULL, descr VARCHAR(80), qty INTEGER)",
        ).unwrap();
        execute(&stmts[0], &mut cache, &mut tsm).unwrap();

        // SYSCOLUMNS should have 3 new rows for PARTS.
        let stmts = parser::parse(
            "SELECT name, typename, nullable FROM SYSCOLUMNS WHERE tabname = 'PARTS'",
        ).unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 3);
        // First column: PARTNO SMALLINT NOT NULL
        assert_eq!(rs.rows[0][0].to_string(), "PARTNO");
        assert_eq!(rs.rows[0][1].to_string(), "SMALLINT");
        assert_eq!(rs.rows[0][2].to_string(), "N"); // not nullable
    }

    #[test]
    fn create_table_delete_and_update() {
        let (mut cache, mut tsm, _dir) = test_fixture("ct_del_upd");
        let stmts = parser::parse(
            "CREATE TABLE kv (k INTEGER NOT NULL, v VARCHAR(20))",
        ).unwrap();
        execute(&stmts[0], &mut cache, &mut tsm).unwrap();

        // Insert two rows.
        for sql in [
            "INSERT INTO kv VALUES (1, 'alpha')",
            "INSERT INTO kv VALUES (2, 'beta')",
        ] {
            let stmts = parser::parse(sql).unwrap();
            execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        }

        // UPDATE one row.
        let stmts = parser::parse("UPDATE kv SET v = 'gamma' WHERE k = 1").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "1");

        // DELETE one row.
        let stmts = parser::parse("DELETE FROM kv WHERE k = 2").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows[0][0].to_string(), "1");

        // Verify final state.
        let stmts = parser::parse("SELECT * FROM kv").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][1].to_string(), "gamma");
    }

    #[test]
    fn error_empty_input() {
        let err = parser::parse("");
        assert!(err.is_ok());
        assert!(err.unwrap().is_empty());
    }

    #[test]
    fn error_insert_value_list_mismatch() {
        let (mut cache, mut tsm, _dir) = test_fixture("err_val_cnt");
        // SYSTABLESPACES has 7 columns but we provide only 2 values.
        let stmts = parser::parse(
            "INSERT INTO SYSTABLESPACES VALUES (1, 'X')",
        )
        .unwrap();
        assert_sqlstate(
            execute(&stmts[0], &mut cache, &mut tsm),
            SqlState::InsertValueListMismatch,
        );
    }

    #[test]
    fn error_not_null_violation() {
        let (mut cache, mut tsm, _dir) = test_fixture("err_null");
        // SYSSCHEMAS has 1 column (NAME VARCHAR). Insert NULL.
        let stmts = parser::parse(
            "INSERT INTO SYSSCHEMAS VALUES (NULL)",
        )
        .unwrap();
        assert_sqlstate(
            execute(&stmts[0], &mut cache, &mut tsm),
            SqlState::NotNullViolation,
        );
    }

    #[test]
    fn error_type_mismatch() {
        let (mut cache, mut tsm, _dir) = test_fixture("err_type");
        // SYSTABLESPACES first column is SMALLINT. Insert a string.
        let stmts = parser::parse(
            "INSERT INTO SYSTABLESPACES VALUES ('not_a_number', 'X', 'D', 'A', 4096, 'N', 1)",
        )
        .unwrap();
        assert_sqlstate(
            execute(&stmts[0], &mut cache, &mut tsm),
            SqlState::AssignmentError,
        );
    }

    // ── DROP TABLE tests ──

    #[test]
    fn drop_table_basic() {
        let (mut cache, mut tsm, dir) = test_fixture("dt_basic");

        // Create and populate a table.
        for sql in [
            "CREATE TABLE things (id INTEGER NOT NULL, name VARCHAR(30))",
            "INSERT INTO things VALUES (1, 'alpha')",
            "INSERT INTO things VALUES (2, 'beta')",
        ] {
            let stmts = parser::parse(sql).unwrap();
            execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        }

        // Verify the table exists.
        let stmts = parser::parse("SELECT * FROM things").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 2);

        // DROP TABLE.
        let stmts = parser::parse("DROP TABLE things").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert!(rs.rows[0][0].to_string().contains("DROPPED"));

        // Table should no longer be queryable.
        let stmts = parser::parse("SELECT * FROM things").unwrap();
        assert_sqlstate(
            execute(&stmts[0], &mut cache, &mut tsm),
            SqlState::TableNotFound,
        );

        // Catalog should not list the table.
        let stmts = parser::parse(
            "SELECT * FROM SYSTABLES WHERE name = 'THINGS'",
        ).unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 0);

        // Columns should be gone from SYSCOLUMNS.
        let stmts = parser::parse(
            "SELECT * FROM SYSCOLUMNS WHERE tabname = 'THINGS'",
        ).unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 0);

        // Data files should be deleted.
        let dat = dir.0.join("PUBLIC.THINGS.DAT");
        let fsm = dir.0.join("PUBLIC.THINGS.FSM");
        assert!(!dat.exists(), ".DAT file should be deleted");
        assert!(!fsm.exists(), ".FSM file should be deleted");
    }

    #[test]
    fn drop_table_not_found() {
        let (mut cache, mut tsm, _dir) = test_fixture("dt_notfound");
        let stmts = parser::parse("DROP TABLE nonexistent").unwrap();
        assert_sqlstate(
            execute(&stmts[0], &mut cache, &mut tsm),
            SqlState::TableNotFound,
        );
    }

    #[test]
    fn drop_table_if_exists_no_error() {
        let (mut cache, mut tsm, _dir) = test_fixture("dt_ifexists");
        let stmts = parser::parse("DROP TABLE IF EXISTS nonexistent").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert!(rs.rows[0][0].to_string().contains("skipped"));
    }

    #[test]
    fn drop_table_system_table_rejected() {
        let (mut cache, mut tsm, _dir) = test_fixture("dt_systable");
        let stmts = parser::parse("DROP TABLE RQSYS.SYSTABLES").unwrap();
        assert_sqlstate(
            execute(&stmts[0], &mut cache, &mut tsm),
            SqlState::SystemSchemaViolation,
        );
    }

    #[test]
    fn drop_table_then_recreate() {
        let (mut cache, mut tsm, _dir) = test_fixture("dt_recreate");

        // Create, drop, and recreate the same table.
        let stmts = parser::parse(
            "CREATE TABLE temp (x INTEGER)",
        ).unwrap();
        execute(&stmts[0], &mut cache, &mut tsm).unwrap();

        let stmts = parser::parse("DROP TABLE temp").unwrap();
        execute(&stmts[0], &mut cache, &mut tsm).unwrap();

        let stmts = parser::parse(
            "CREATE TABLE temp (y VARCHAR(20))",
        ).unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert!(rs.rows[0][0].to_string().contains("CREATED"));

        // New table should have column Y, not X.
        let stmts = parser::parse(
            "SELECT name FROM SYSCOLUMNS WHERE tabname = 'TEMP'",
        ).unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0].to_string(), "Y");

        // Should be usable.
        let stmts = parser::parse("INSERT INTO temp VALUES ('hello')").unwrap();
        execute(&stmts[0], &mut cache, &mut tsm).unwrap();

        let stmts = parser::parse("SELECT * FROM temp").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0].to_string(), "hello");
    }

    #[test]
    fn drop_table_with_schema() {
        let (mut cache, mut tsm, _dir) = test_fixture("dt_schema");

        let stmts = parser::parse(
            "CREATE TABLE myns.data (val INTEGER)",
        ).unwrap();
        execute(&stmts[0], &mut cache, &mut tsm).unwrap();

        let stmts = parser::parse("DROP TABLE myns.data").unwrap();
        let rs = execute(&stmts[0], &mut cache, &mut tsm).unwrap();
        assert!(rs.rows[0][0].to_string().contains("DROPPED"));

        let stmts = parser::parse("SELECT * FROM myns.data").unwrap();
        assert_sqlstate(
            execute(&stmts[0], &mut cache, &mut tsm),
            SqlState::TableNotFound,
        );
    }
}
