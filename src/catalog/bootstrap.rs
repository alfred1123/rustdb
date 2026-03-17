use std::fs;
use std::path::Path;

use crate::catalog::row::RowWriter;
use crate::error::Result;
use crate::storage::page::DEFAULT_PAGE_SIZE;

const SCHEMA: &str = "RQSYS";

/// Create a fresh database with system catalog tables.
pub fn bootstrap(data_dir: &Path, text_mode: bool) -> Result<()> {
    for dir in ["systbsp", "usertbsp", "temptbsp", "log", "admin", "backups"] {
        fs::create_dir_all(data_dir.join(dir))?;
    }
    log::debug!("created directory structure under {}", data_dir.display());

    let systbsp = data_dir.join("systbsp");
    write_systablespaces(&systbsp, text_mode)?;
    write_sysschemas(&systbsp, text_mode)?;
    write_systables(&systbsp, text_mode)?;
    write_syscolumns(&systbsp, text_mode)?;

    log::info!("bootstrap complete: 4 catalog tables written");
    Ok(())
}

fn write_dat(dir: &Path, table: &str, header: &str, text_rows: &[String], binary_rows: &[Vec<u8>], text_mode: bool) -> Result<()> {
    let path = dir.join(format!("{SCHEMA}.{table}.0.DAT"));
    if text_mode {
        let mut content = String::from(header);
        content.push('\n');
        for row in text_rows {
            content.push_str(row);
            content.push('\n');
        }
        fs::write(path, content)?;
    } else {
        let mut buf = Vec::new();
        for row in binary_rows {
            buf.extend_from_slice(&(row.len() as u64).to_le_bytes());
            buf.extend_from_slice(row);
        }
        fs::write(path, buf)?;
    }
    Ok(())
}

fn write_systablespaces(dir: &Path, text_mode: bool) -> Result<()> {
    let data: [(i16, &str, &str, &str); 3] = [
        (1, "SYSTBSP", "D", "N"),
        (2, "USERTBSP", "D", "N"),
        (3, "TEMPTBSP", "T", "N"),
    ];

    let text_rows: Vec<String> = data.iter()
        .map(|(id, name, t, state)| format!("{id}\t{name}\t{t}\t{}\t{state}", DEFAULT_PAGE_SIZE))
        .collect();

    let binary_rows: Vec<Vec<u8>> = data.iter()
        .map(|(id, name, t, state)| {
            let mut w = RowWriter::new();
            w.write_i16(*id);
            w.write_string(name);
            w.write_string(t);
            w.write_i32(DEFAULT_PAGE_SIZE as i32);
            w.write_string(state);
            w.finish()
        })
        .collect();

    write_dat(dir, "SYSTABLESPACES", "ID\tNAME\tTYPE\tPAGE_SIZE\tSTATE", &text_rows, &binary_rows, text_mode)
}

fn write_sysschemas(dir: &Path, text_mode: bool) -> Result<()> {
    let mut w = RowWriter::new();
    w.write_string(SCHEMA);
    write_dat(dir, "SYSSCHEMAS", "NAME", &[SCHEMA.to_string()], &[w.finish()], text_mode)
}

fn write_systables(dir: &Path, text_mode: bool) -> Result<()> {
    let tables: [(&str, i16); 4] = [
        ("SYSTABLESPACES", 5),
        ("SYSSCHEMAS", 1),
        ("SYSTABLES", 4),
        ("SYSCOLUMNS", 6),
    ];

    let text_rows: Vec<String> = tables.iter()
        .map(|(name, cc)| format!("{name}\t{SCHEMA}\t1\t{cc}"))
        .collect();

    let binary_rows: Vec<Vec<u8>> = tables.iter()
        .map(|(name, col_count)| {
            let mut w = RowWriter::new();
            w.write_string(name);
            w.write_string(SCHEMA);
            w.write_i16(1);
            w.write_i16(*col_count);
            w.finish()
        })
        .collect();

    write_dat(dir, "SYSTABLES", "NAME\tSCHEMA_NAME\tTABLESPACE_ID\tCOL_COUNT", &text_rows, &binary_rows, text_mode)
}

fn write_syscolumns(dir: &Path, text_mode: bool) -> Result<()> {
    let cols: &[(&str, &str, i16, &str, bool)] = &[
        ("ID", "SYSTABLESPACES", 0, "SMALLINT", false),
        ("NAME", "SYSTABLESPACES", 1, "VARCHAR(128)", false),
        ("TYPE", "SYSTABLESPACES", 2, "CHAR(1)", false),
        ("PAGE_SIZE", "SYSTABLESPACES", 3, "INTEGER", false),
        ("STATE", "SYSTABLESPACES", 4, "CHAR(1)", false),
        ("NAME", "SYSSCHEMAS", 0, "VARCHAR(128)", false),
        ("NAME", "SYSTABLES", 0, "VARCHAR(128)", false),
        ("SCHEMA_NAME", "SYSTABLES", 1, "VARCHAR(128)", false),
        ("TABLESPACE_ID", "SYSTABLES", 2, "SMALLINT", false),
        ("COL_COUNT", "SYSTABLES", 3, "SMALLINT", false),
        ("NAME", "SYSCOLUMNS", 0, "VARCHAR(128)", false),
        ("TABLE_NAME", "SYSCOLUMNS", 1, "VARCHAR(128)", false),
        ("SCHEMA_NAME", "SYSCOLUMNS", 2, "VARCHAR(128)", false),
        ("ORDINAL", "SYSCOLUMNS", 3, "SMALLINT", false),
        ("TYPE_NAME", "SYSCOLUMNS", 4, "VARCHAR(20)", false),
        ("NULLABLE", "SYSCOLUMNS", 5, "CHAR(1)", false),
    ];

    let text_rows: Vec<String> = cols.iter()
        .map(|(name, table, ord, tn, nullable)| {
            let flag = if *nullable { "Y" } else { "N" };
            format!("{name}\t{table}\t{SCHEMA}\t{ord}\t{tn}\t{flag}")
        })
        .collect();

    let binary_rows: Vec<Vec<u8>> = cols.iter()
        .map(|(name, table, ordinal, type_name, nullable)| {
            let mut w = RowWriter::new();
            w.write_string(name);
            w.write_string(table);
            w.write_string(SCHEMA);
            w.write_i16(*ordinal);
            w.write_string(type_name);
            w.write_bool(*nullable);
            w.finish()
        })
        .collect();

    write_dat(dir, "SYSCOLUMNS", "NAME\tTABLE_NAME\tSCHEMA_NAME\tORDINAL\tTYPE_NAME\tNULLABLE", &text_rows, &binary_rows, text_mode)
}
