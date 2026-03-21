use std::fs;
use std::path::Path;

use crate::catalog::config::DbConfig;
use crate::catalog::row::RowReader;
use crate::catalog::types::*;
use crate::error::{Error, Result};
use crate::storage::heap::HeapFile;

/// Load the full system catalog from a database directory.
pub fn load_catalog(data_dir: &Path, config: &DbConfig) -> Result<Catalog> {
    log::debug!("loading catalog from {}", data_dir.display());
    let systbsp = data_dir.join("systbsp");
    let catalog = Catalog {
        tablespaces: load_tablespaces(&systbsp, config)?,
        schemas: load_schemas(&systbsp, config)?,
        tables: load_tables(&systbsp, config)?,
        columns: load_columns(&systbsp, config)?,
        bufferpools: load_bufferpools(&systbsp, config)?,
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

fn read_binary_rows(dir: &Path, table: &str, config: &DbConfig) -> Result<Vec<Vec<u8>>> {
    let path = dir.join(format!("{}.{table}.0.DAT", config.sys_schema));
    if !path.exists() {
        return Err(Error::Catalog(format!("catalog file not found: {}", path.display())));
    }
    let mut heap = HeapFile::open(&path, config.page_size)?;
    let rows = heap.scan()?;
    Ok(rows.into_iter().map(|(_, data)| data).collect())
}

fn read_text_rows(dir: &Path, table: &str, config: &DbConfig) -> Result<Vec<Vec<String>>> {
    let path = dir.join(format!("{}.{table}.0.DAT", config.sys_schema));
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

fn load_tablespaces(dir: &Path, config: &DbConfig) -> Result<Vec<Tablespace>> {
    if config.text_mode {
        read_text_rows(dir, "SYSTABLESPACES", config)?
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
        read_binary_rows(dir, "SYSTABLESPACES", config)?
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

fn load_schemas(dir: &Path, config: &DbConfig) -> Result<Vec<Schema>> {
    if config.text_mode {
        read_text_rows(dir, "SYSSCHEMAS", config)?
            .iter()
            .map(|r| Ok(Schema { name: col(r, 0, "SYSSCHEMAS")? }))
            .collect()
    } else {
        read_binary_rows(dir, "SYSSCHEMAS", config)?
            .iter()
            .map(|row| {
                let mut r = RowReader::new(row);
                Ok(Schema { name: r.read_string()? })
            })
            .collect()
    }
}

fn load_tables(dir: &Path, config: &DbConfig) -> Result<Vec<Table>> {
    if config.text_mode {
        read_text_rows(dir, "SYSTABLES", config)?
            .iter()
            .map(|r| Ok(Table {
                tableid: parse_i32(&col(r, 0, "SYSTABLES")?)?,
                name: col(r, 1, "SYSTABLES")?,
                schemaname: col(r, 2, "SYSTABLES")?,
                tbspaceid: parse_i16(&col(r, 3, "SYSTABLES")?)?,
                colcount: parse_i16(&col(r, 4, "SYSTABLES")?)?,
            }))
            .collect()
    } else {
        read_binary_rows(dir, "SYSTABLES", config)?
            .iter()
            .map(|row| {
                let mut r = RowReader::new(row);
                Ok(Table {
                    tableid: r.read_i32()?,
                    name: r.read_string()?,
                    schemaname: r.read_string()?,
                    tbspaceid: r.read_i16()?,
                    colcount: r.read_i16()?,
                })
            })
            .collect()
    }
}

fn load_columns(dir: &Path, config: &DbConfig) -> Result<Vec<Column>> {
    if config.text_mode {
        read_text_rows(dir, "SYSCOLUMNS", config)?
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
        read_binary_rows(dir, "SYSCOLUMNS", config)?
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

fn load_bufferpools(dir: &Path, config: &DbConfig) -> Result<Vec<BufferPool>> {
    if config.text_mode {
        read_text_rows(dir, "SYSBUFFERPOOLS", config)?
            .iter()
            .map(|r| Ok(BufferPool {
                bpid: parse_i32(&col(r, 0, "SYSBUFFERPOOLS")?)?,
                bpname: col(r, 1, "SYSBUFFERPOOLS")?,
                pagesize: parse_i32(&col(r, 2, "SYSBUFFERPOOLS")?)?,
                npages: parse_i32(&col(r, 3, "SYSBUFFERPOOLS")?)?,
            }))
            .collect()
    } else {
        read_binary_rows(dir, "SYSBUFFERPOOLS", config)?
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
