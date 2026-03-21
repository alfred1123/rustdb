use std::fs;
use std::path::Path;

use crate::catalog::config::DbConfig;
use crate::catalog::row::RowWriter;
use crate::error::Result;
use crate::storage::heap::HeapFile;

/// Create a fresh database with system catalog tables.
pub fn bootstrap(data_dir: &Path, config: &DbConfig) -> Result<()> {
    for dir in ["systbsp", "usertbsp", "temptbsp", "log", "admin", "backups"] {
        fs::create_dir_all(data_dir.join(dir))?;
    }
    log::debug!("created directory structure under {}", data_dir.display());

    config.write(data_dir)?;

    let systbsp = data_dir.join("systbsp");
    write_systablespaces(&systbsp, config)?;
    write_sysschemas(&systbsp, config)?;
    write_systables(&systbsp, config)?;
    write_syscolumns(&systbsp, config)?;
    write_sysbufferpools(&systbsp, config)?;

    log::info!("bootstrap complete: SQLDBCONF + 5 catalog tables written");
    Ok(())
}

fn write_dat(dir: &Path, table: &str, header: &str, text_rows: &[String], binary_rows: &[Vec<u8>], config: &DbConfig) -> Result<()> {
    let path = dir.join(format!("{}.{table}.0.DAT", config.sys_schema));
    if config.text_mode {
        let mut content = String::from(header);
        content.push('\n');
        for row in text_rows {
            content.push_str(row);
            content.push('\n');
        }
        fs::write(path, content)?;
    } else {
        let mut heap = HeapFile::open(&path, config.page_size)?;
        for row in binary_rows {
            heap.insert_row(row)?;
        }
        heap.save_fsm()?;
    }
    Ok(())
}

fn write_systablespaces(dir: &Path, config: &DbConfig) -> Result<()> {
    let ps = config.page_size as i32;
    // (tbspaceid, tbspace, tbspacetype, datatype, pagesize, state, bufferpoolid)
    let data: [(i32, &str, &str, &str, &str, i32); 3] = [
        (1, "SYSTBSP", "S", "A", "N", 1),
        (2, "USERTBSP", "D", "A", "N", 1),
        (3, "TEMPTBSP", "D", "T", "N", 4),
    ];

    let text_rows: Vec<String> = data.iter()
        .map(|(id, name, tt, dt, state, bpid)| format!("{id}\t{name}\t{tt}\t{dt}\t{ps}\t{state}\t{bpid}"))
        .collect();

    let binary_rows: Vec<Vec<u8>> = data.iter()
        .map(|(id, name, tt, dt, state, bpid)| {
            let mut w = RowWriter::new();
            w.write_i32(*id);
            w.write_string(name);
            w.write_string(tt);
            w.write_string(dt);
            w.write_i32(ps);
            w.write_string(state);
            w.write_i32(*bpid);
            w.finish()
        })
        .collect();

    write_dat(dir, "SYSTABLESPACES", "TBSPACEID\tTBSPACE\tTBSPACETYPE\tDATATYPE\tPAGESIZE\tSTATE\tBUFFERPOOLID", &text_rows, &binary_rows, config)
}

fn write_sysschemas(dir: &Path, config: &DbConfig) -> Result<()> {
    let schemas = [config.sys_schema.as_str(), "PUBLIC"];
    let text_rows: Vec<String> = schemas.iter().map(|s| s.to_string()).collect();
    let binary_rows: Vec<Vec<u8>> = schemas.iter().map(|s| {
        let mut w = RowWriter::new();
        w.write_string(s);
        w.finish()
    }).collect();
    write_dat(dir, "SYSSCHEMAS", "NAME", &text_rows, &binary_rows, config)
}

fn write_systables(dir: &Path, config: &DbConfig) -> Result<()> {
    // (tableid, name, colcount) — IDs assigned sequentially starting at 1.
    let tables: [(i32, &str, i16); 5] = [
        (1, "SYSTABLESPACES", 7i16),
        (2, "SYSSCHEMAS", 1),
        (3, "SYSTABLES", 5),
        (4, "SYSCOLUMNS", 6),
        (5, "SYSBUFFERPOOLS", 4),
    ];

    let text_rows: Vec<String> = tables.iter()
        .map(|(id, name, cc)| format!("{id}\t{name}\t{}\t1\t{cc}", config.sys_schema))
        .collect();

    let binary_rows: Vec<Vec<u8>> = tables.iter()
        .map(|(id, name, col_count)| {
            let mut w = RowWriter::new();
            w.write_i32(*id);
            w.write_string(name);
            w.write_string(&config.sys_schema);
            w.write_i16(1);
            w.write_i16(*col_count);
            w.finish()
        })
        .collect();

    write_dat(dir, "SYSTABLES", "TABLEID\tNAME\tSCHEMANAME\tTBSPACEID\tCOLCOUNT", &text_rows, &binary_rows, config)
}

fn write_syscolumns(dir: &Path, config: &DbConfig) -> Result<()> {
    let cols: &[(&str, &str, i16, &str, bool)] = &[
        ("TBSPACEID", "SYSTABLESPACES", 0, "INTEGER", false),
        ("TBSPACE", "SYSTABLESPACES", 1, "VARCHAR(128)", false),
        ("TBSPACETYPE", "SYSTABLESPACES", 2, "CHAR(1)", false),
        ("DATATYPE", "SYSTABLESPACES", 3, "CHAR(1)", false),
        ("PAGESIZE", "SYSTABLESPACES", 4, "INTEGER", false),
        ("STATE", "SYSTABLESPACES", 5, "CHAR(1)", false),
        ("BUFFERPOOLID", "SYSTABLESPACES", 6, "INTEGER", false),
        ("NAME", "SYSSCHEMAS", 0, "VARCHAR(128)", false),
        ("TABLEID", "SYSTABLES", 0, "INTEGER", false),
        ("NAME", "SYSTABLES", 1, "VARCHAR(128)", false),
        ("SCHEMANAME", "SYSTABLES", 2, "VARCHAR(128)", false),
        ("TBSPACEID", "SYSTABLES", 3, "SMALLINT", false),
        ("COLCOUNT", "SYSTABLES", 4, "SMALLINT", false),
        ("NAME", "SYSCOLUMNS", 0, "VARCHAR(128)", false),
        ("TABNAME", "SYSCOLUMNS", 1, "VARCHAR(128)", false),
        ("SCHEMANAME", "SYSCOLUMNS", 2, "VARCHAR(128)", false),
        ("ORDINAL", "SYSCOLUMNS", 3, "SMALLINT", false),
        ("TYPENAME", "SYSCOLUMNS", 4, "VARCHAR(20)", false),
        ("NULLABLE", "SYSCOLUMNS", 5, "CHAR(1)", false),
        ("BPID", "SYSBUFFERPOOLS", 0, "INTEGER", false),
        ("BPNAME", "SYSBUFFERPOOLS", 1, "VARCHAR(128)", false),
        ("PAGESIZE", "SYSBUFFERPOOLS", 2, "INTEGER", false),
        ("NPAGES", "SYSBUFFERPOOLS", 3, "INTEGER", false),
    ];

    let text_rows: Vec<String> = cols.iter()
        .map(|(name, table, ord, tn, nullable)| {
            let flag = if *nullable { "Y" } else { "N" };
            format!("{name}\t{table}\t{}\t{ord}\t{tn}\t{flag}", config.sys_schema)
        })
        .collect();

    let binary_rows: Vec<Vec<u8>> = cols.iter()
        .map(|(name, table, ordinal, type_name, nullable)| {
            let mut w = RowWriter::new();
            w.write_string(name);
            w.write_string(table);
            w.write_string(&config.sys_schema);
            w.write_i16(*ordinal);
            w.write_string(type_name);
            w.write_bool(*nullable);
            w.finish()
        })
        .collect();

    write_dat(dir, "SYSCOLUMNS", "NAME\tTABNAME\tSCHEMANAME\tORDINAL\tTYPENAME\tNULLABLE", &text_rows, &binary_rows, config)
}

fn write_sysbufferpools(dir: &Path, config: &DbConfig) -> Result<()> {
    let ps = config.page_size as i32;
    // (bpid, bpname, pagesize, npages)
    let data: [(i32, &str, i32, i32); 4] = [
        (1, "RQDEFAULTBP", ps, 128),
        (2, "INDEXBP", ps, 64),
        (3, "LOBBP", ps * 8, 32),
        (4, "TEMPBP", ps, 64),
    ];

    let text_rows: Vec<String> = data.iter()
        .map(|(id, name, pgsz, np)| format!("{id}\t{name}\t{pgsz}\t{np}"))
        .collect();

    let binary_rows: Vec<Vec<u8>> = data.iter()
        .map(|(id, name, pgsz, np)| {
            let mut w = RowWriter::new();
            w.write_i32(*id);
            w.write_string(name);
            w.write_i32(*pgsz);
            w.write_i32(*np);
            w.finish()
        })
        .collect();

    write_dat(dir, "SYSBUFFERPOOLS", "BPID\tBPNAME\tPAGESIZE\tNPAGES", &text_rows, &binary_rows, config)
}
