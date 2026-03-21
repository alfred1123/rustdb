use std::fs;
use std::path::Path;

use crate::catalog::row::RowReader;
use crate::catalog::types::*;
use crate::error::{Error, Result};
use crate::storage::heap::HeapFile;

use super::SYSTEM_SCHEMA;

/// Load the full system catalog from a database directory.
pub fn load_catalog(data_dir: &Path, text_mode: bool, page_size: usize) -> Result<Catalog> {
    log::debug!("loading catalog from {}", data_dir.display());
    let systbsp = data_dir.join("systbsp");
    let catalog = Catalog {
        tablespaces: load_tablespaces(&systbsp, text_mode, page_size)?,
        schemas: load_schemas(&systbsp, text_mode, page_size)?,
        tables: load_tables(&systbsp, text_mode, page_size)?,
        columns: load_columns(&systbsp, text_mode, page_size)?,
        bufferpools: load_bufferpools(&systbsp, text_mode, page_size)?,
    };
    log::info!(
        "catalog loaded: {} tablespaces, {} schemas, {} tables, {} columns, {} bufferpools",
        catalog.tablespaces.len(),
        catalog.schemas.len(),
        catalog.tables.len(),
        catalog.columns.len(),
        catalog.bufferpools.len(),
    );
    Ok(catalog)
}

// ── Shared helpers ──

fn read_binary_rows(dir: &Path, table: &str, page_size: usize) -> Result<Vec<Vec<u8>>> {
    let path = dir.join(format!("{SYSTEM_SCHEMA}.{table}.0.DAT"));
    if !path.exists() {
        return Err(Error::Catalog(format!("catalog file not found: {}", path.display())));
    }
    let mut heap = HeapFile::open(&path, page_size)?;
    let rows = heap.scan()?;
    Ok(rows.into_iter().map(|(_, data)| data).collect())
}

fn read_text_rows(dir: &Path, table: &str) -> Result<Vec<Vec<String>>> {
    let path = dir.join(format!("{SYSTEM_SCHEMA}.{table}.0.DAT"));
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

fn load_tablespaces(dir: &Path, text_mode: bool, page_size: usize) -> Result<Vec<Tablespace>> {
    if text_mode {
        read_text_rows(dir, "SYSTABLESPACES")?
            .iter()
            .map(|r| Ok(Tablespace {
                tbspaceid: parse_i32(&col(r, 0, "SYSTABLESPACES")?)?,
                tbspace: col(r, 1, "SYSTABLESPACES")?,
                tbspacetype: col(r, 2, "SYSTABLESPACES")?,
                datatype: col(r, 3, "SYSTABLESPACES")?,
                pagesize: parse_i32(&col(r, 4, "SYSTABLESPACES")?)?,
                state: col(r, 5, "SYSTABLESPACES")?,
                bufferpoolid: parse_i32(&col(r, 6, "SYSTABLESPACES")?)?,
            }))
            .collect()
    } else {
        read_binary_rows(dir, "SYSTABLESPACES", page_size)?
            .iter()
            .map(|row| {
                let mut r = RowReader::new(row);
                Ok(Tablespace {
                    tbspaceid: r.read_i32()?,
                    tbspace: r.read_string()?,
                    tbspacetype: r.read_string()?,
                    datatype: r.read_string()?,
                    pagesize: r.read_i32()?,
                    state: r.read_string()?,
                    bufferpoolid: r.read_i32()?,
                })
            })
            .collect()
    }
}

fn load_schemas(dir: &Path, text_mode: bool, page_size: usize) -> Result<Vec<Schema>> {
    if text_mode {
        read_text_rows(dir, "SYSSCHEMAS")?
            .iter()
            .map(|r| Ok(Schema { name: col(r, 0, "SYSSCHEMAS")? }))
            .collect()
    } else {
        read_binary_rows(dir, "SYSSCHEMAS", page_size)?
            .iter()
            .map(|row| {
                let mut r = RowReader::new(row);
                Ok(Schema { name: r.read_string()? })
            })
            .collect()
    }
}

fn load_tables(dir: &Path, text_mode: bool, page_size: usize) -> Result<Vec<Table>> {
    if text_mode {
        read_text_rows(dir, "SYSTABLES")?
            .iter()
            .map(|r| Ok(Table {
                name: col(r, 0, "SYSTABLES")?,
                schemaname: col(r, 1, "SYSTABLES")?,
                tbspaceid: parse_i16(&col(r, 2, "SYSTABLES")?)?,
                colcount: parse_i16(&col(r, 3, "SYSTABLES")?)?,
            }))
            .collect()
    } else {
        read_binary_rows(dir, "SYSTABLES", page_size)?
            .iter()
            .map(|row| {
                let mut r = RowReader::new(row);
                Ok(Table {
                    name: r.read_string()?,
                    schemaname: r.read_string()?,
                    tbspaceid: r.read_i16()?,
                    colcount: r.read_i16()?,
                })
            })
            .collect()
    }
}

fn load_columns(dir: &Path, text_mode: bool, page_size: usize) -> Result<Vec<Column>> {
    if text_mode {
        read_text_rows(dir, "SYSCOLUMNS")?
            .iter()
            .map(|r| Ok(Column {
                name: col(r, 0, "SYSCOLUMNS")?,
                tabname: col(r, 1, "SYSCOLUMNS")?,
                schemaname: col(r, 2, "SYSCOLUMNS")?,
                ordinal: parse_i16(&col(r, 3, "SYSCOLUMNS")?)?,
                typename: col(r, 4, "SYSCOLUMNS")?,
                nullable: col(r, 5, "SYSCOLUMNS")? == "Y",
            }))
            .collect()
    } else {
        read_binary_rows(dir, "SYSCOLUMNS", page_size)?
            .iter()
            .map(|row| {
                let mut r = RowReader::new(row);
                Ok(Column {
                    name: r.read_string()?,
                    tabname: r.read_string()?,
                    schemaname: r.read_string()?,
                    ordinal: r.read_i16()?,
                    typename: r.read_string()?,
                    nullable: r.read_bool()?,
                })
            })
            .collect()
    }
}

fn load_bufferpools(dir: &Path, text_mode: bool, page_size: usize) -> Result<Vec<BufferPool>> {
    if text_mode {
        read_text_rows(dir, "SYSBUFFERPOOLS")?
            .iter()
            .map(|r| Ok(BufferPool {
                bpid: parse_i32(&col(r, 0, "SYSBUFFERPOOLS")?)?,
                bpname: col(r, 1, "SYSBUFFERPOOLS")?,
                pagesize: parse_i32(&col(r, 2, "SYSBUFFERPOOLS")?)?,
                npages: parse_i32(&col(r, 3, "SYSBUFFERPOOLS")?)?,
            }))
            .collect()
    } else {
        read_binary_rows(dir, "SYSBUFFERPOOLS", page_size)?
            .iter()
            .map(|row| {
                let mut r = RowReader::new(row);
                Ok(BufferPool {
                    bpid: r.read_i32()?,
                    bpname: r.read_string()?,
                    pagesize: r.read_i32()?,
                    npages: r.read_i32()?,
                })
            })
            .collect()
    }
}
