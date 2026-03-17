use std::fs;
use std::path::Path;

use crate::catalog::row::RowReader;
use crate::catalog::types::*;
use crate::error::{Error, Result};

const SCHEMA: &str = "RQSYS";

/// Load the full system catalog from a database directory.
pub fn load_catalog(data_dir: &Path, text_mode: bool) -> Result<Catalog> {
    log::debug!("loading catalog from {}", data_dir.display());
    let systbsp = data_dir.join("systbsp");
    let catalog = Catalog {
        tablespaces: load_tablespaces(&systbsp, text_mode)?,
        schemas: load_schemas(&systbsp, text_mode)?,
        tables: load_tables(&systbsp, text_mode)?,
        columns: load_columns(&systbsp, text_mode)?,
    };
    log::info!(
        "catalog loaded: {} tablespaces, {} schemas, {} tables, {} columns",
        catalog.tablespaces.len(),
        catalog.schemas.len(),
        catalog.tables.len(),
        catalog.columns.len(),
    );
    Ok(catalog)
}

// ── Shared helpers ──

fn read_binary_rows(dir: &Path, table: &str) -> Result<Vec<Vec<u8>>> {
    let path = dir.join(format!("{SCHEMA}.{table}.0.DAT"));
    let data = fs::read(&path).map_err(|e| {
        Error::Catalog(format!("failed to read {}: {e}", path.display()))
    })?;
    let mut rows = Vec::new();
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let len = u64::from_le_bytes(
            data[pos..pos + 8].try_into().unwrap(),
        ) as usize;
        pos += 8;
        if pos + len > data.len() {
            return Err(Error::Corruption(format!(
                "row extends past end of {table}.0.DAT"
            )));
        }
        rows.push(data[pos..pos + len].to_vec());
        pos += len;
    }
    Ok(rows)
}

fn read_text_rows(dir: &Path, table: &str) -> Result<Vec<Vec<String>>> {
    let path = dir.join(format!("{SCHEMA}.{table}.0.DAT"));
    let content = fs::read_to_string(&path).map_err(|e| {
        Error::Catalog(format!("failed to read {}: {e}", path.display()))
    })?;
    let mut lines = content.lines();
    lines.next(); // skip header
    Ok(lines
        .filter(|l| !l.is_empty())
        .map(|l| l.split('\t').map(String::from).collect())
        .collect())
}

fn col(row: &[String], idx: usize, table: &str) -> Result<String> {
    row.get(idx)
        .cloned()
        .ok_or_else(|| Error::Corruption(format!("missing column {idx} in {table}")))
}

fn parse_i16(val: &str) -> Result<i16> {
    val.parse().map_err(|e: std::num::ParseIntError| Error::Corruption(e.to_string()))
}

fn parse_i32(val: &str) -> Result<i32> {
    val.parse().map_err(|e: std::num::ParseIntError| Error::Corruption(e.to_string()))
}

// ── Per-table loaders ──

fn load_tablespaces(dir: &Path, text_mode: bool) -> Result<Vec<Tablespace>> {
    if text_mode {
        read_text_rows(dir, "SYSTABLESPACES")?
            .iter()
            .map(|r| Ok(Tablespace {
                id: parse_i16(&col(r, 0, "SYSTABLESPACES")?)?,
                name: col(r, 1, "SYSTABLESPACES")?,
                ts_type: col(r, 2, "SYSTABLESPACES")?,
                page_size: parse_i32(&col(r, 3, "SYSTABLESPACES")?)?,
                state: col(r, 4, "SYSTABLESPACES")?,
            }))
            .collect()
    } else {
        read_binary_rows(dir, "SYSTABLESPACES")?
            .iter()
            .map(|row| {
                let mut r = RowReader::new(row);
                Ok(Tablespace {
                    id: r.read_i16()?,
                    name: r.read_string()?,
                    ts_type: r.read_string()?,
                    page_size: r.read_i32()?,
                    state: r.read_string()?,
                })
            })
            .collect()
    }
}

fn load_schemas(dir: &Path, text_mode: bool) -> Result<Vec<Schema>> {
    if text_mode {
        read_text_rows(dir, "SYSSCHEMAS")?
            .iter()
            .map(|r| Ok(Schema { name: col(r, 0, "SYSSCHEMAS")? }))
            .collect()
    } else {
        read_binary_rows(dir, "SYSSCHEMAS")?
            .iter()
            .map(|row| {
                let mut r = RowReader::new(row);
                Ok(Schema { name: r.read_string()? })
            })
            .collect()
    }
}

fn load_tables(dir: &Path, text_mode: bool) -> Result<Vec<Table>> {
    if text_mode {
        read_text_rows(dir, "SYSTABLES")?
            .iter()
            .map(|r| Ok(Table {
                name: col(r, 0, "SYSTABLES")?,
                schema_name: col(r, 1, "SYSTABLES")?,
                tablespace_id: parse_i16(&col(r, 2, "SYSTABLES")?)?,
                col_count: parse_i16(&col(r, 3, "SYSTABLES")?)?,
            }))
            .collect()
    } else {
        read_binary_rows(dir, "SYSTABLES")?
            .iter()
            .map(|row| {
                let mut r = RowReader::new(row);
                Ok(Table {
                    name: r.read_string()?,
                    schema_name: r.read_string()?,
                    tablespace_id: r.read_i16()?,
                    col_count: r.read_i16()?,
                })
            })
            .collect()
    }
}

fn load_columns(dir: &Path, text_mode: bool) -> Result<Vec<Column>> {
    if text_mode {
        read_text_rows(dir, "SYSCOLUMNS")?
            .iter()
            .map(|r| Ok(Column {
                name: col(r, 0, "SYSCOLUMNS")?,
                table_name: col(r, 1, "SYSCOLUMNS")?,
                schema_name: col(r, 2, "SYSCOLUMNS")?,
                ordinal: parse_i16(&col(r, 3, "SYSCOLUMNS")?)?,
                type_name: col(r, 4, "SYSCOLUMNS")?,
                nullable: col(r, 5, "SYSCOLUMNS")? == "Y",
            }))
            .collect()
    } else {
        read_binary_rows(dir, "SYSCOLUMNS")?
            .iter()
            .map(|row| {
                let mut r = RowReader::new(row);
                Ok(Column {
                    name: r.read_string()?,
                    table_name: r.read_string()?,
                    schema_name: r.read_string()?,
                    ordinal: r.read_i16()?,
                    type_name: r.read_string()?,
                    nullable: r.read_bool()?,
                })
            })
            .collect()
    }
}
