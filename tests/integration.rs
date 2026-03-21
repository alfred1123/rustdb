//! Integration tests — full-stack bootstrap → load → execute SQL → verify.
//!
//! These tests exercise the public API end-to-end.  They live in `tests/`
//! (not `#[cfg(test)]`) so they compile as a separate crate that imports
//! `rqdb` as a library, exactly like an external consumer would.

use std::path::PathBuf;

use rqdb::catalog::bootstrap;
use rqdb::catalog::cache::CatalogCache;
use rqdb::catalog::config::DbConfig;
use rqdb::catalog::loader;
use rqdb::sql::{executor, parser};
use rqdb::sql::types::Value;
use rqdb::storage::tablespace::TablespaceManager;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct TempDb(PathBuf);

impl TempDb {
    /// Bootstrap a fresh database in a temporary directory.
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("rqdb_integ_{name}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Shorthand: bootstrap + load + build cache + open TSM.
fn open_db(name: &str) -> (CatalogCache, TablespaceManager, TempDb) {
    let db = TempDb::new(name);
    let cfg = DbConfig::default();
    bootstrap::bootstrap(db.path(), &cfg).unwrap();
    let catalog = loader::load_catalog(db.path(), &cfg).unwrap();
    let cache = CatalogCache::new(catalog, cfg);
    let tsm = TablespaceManager::open(db.path(), &cache).unwrap();
    (cache, tsm, db)
}

/// Shorthand: reload a database from an existing directory.
fn reload_db(db: &TempDb) -> (CatalogCache, TablespaceManager) {
    let cfg = DbConfig::read(db.path()).unwrap();
    let catalog = loader::load_catalog(db.path(), &cfg).unwrap();
    let cache = CatalogCache::new(catalog, cfg);
    let tsm = TablespaceManager::open(db.path(), &cache).unwrap();
    (cache, tsm)
}

/// Execute a SQL string and return the ResultSet.
fn run(sql: &str, cache: &mut CatalogCache, tsm: &mut TablespaceManager) -> rqdb::error::Result<rqdb::sql::types::ResultSet> {
    let stmts = parser::parse(sql).map_err(|e| {
        rqdb::error::sql_error(rqdb::error::SqlState::ParseError, e.to_string())
    })?;
    executor::execute(&stmts[0], cache, tsm)
}

// ---------------------------------------------------------------------------
// Tests: Bootstrap & Catalog
// ---------------------------------------------------------------------------

#[test]
fn bootstrap_creates_sqldbconf_and_catalog() {
    let (cache, _tsm, _db) = open_db("boot_cat");

    // SQLDBCONF should exist
    assert!(_db.path().join("admin/SQLDBCONF").exists());

    // System tablespaces present (IDs start at 1)
    assert!(cache.get_tablespace_by_id(1).is_some()); // SYSTBSP
    assert!(cache.get_tablespace_by_id(2).is_some()); // USERTBSP
    assert!(cache.get_tablespace_by_id(3).is_some()); // TEMPTBSP

    // System schemas present
    assert!(cache.has_schema(&cache.config().sys_schema));
    assert!(cache.has_schema("PUBLIC"));

    // System tables present
    let sys = &cache.config().sys_schema;
    assert!(cache.get_table(sys, "SYSTABLESPACES").is_some());
    assert!(cache.get_table(sys, "SYSTABLES").is_some());
    assert!(cache.get_table(sys, "SYSCOLUMNS").is_some());
    assert!(cache.get_table(sys, "SYSSCHEMAS").is_some());
    assert!(cache.get_table(sys, "SYSBUFFERPOOLS").is_some());
}

#[test]
fn select_catalog_tables_after_bootstrap() {
    let (mut cache, mut tsm, _db) = open_db("sel_cat");

    let rs = run("SELECT * FROM SYSTABLES", &mut cache, &mut tsm).unwrap();
    // At least the 5 system tables
    assert!(rs.rows.len() >= 5, "expected >=5 catalog tables, got {}", rs.rows.len());
}

// ---------------------------------------------------------------------------
// Tests: DDL + DML workflow
// ---------------------------------------------------------------------------

#[test]
fn create_table_and_insert_select() {
    let (mut cache, mut tsm, _db) = open_db("ddl_dml");

    // CREATE TABLE
    run("CREATE TABLE t1 (id INTEGER NOT NULL, name VARCHAR(50))", &mut cache, &mut tsm).unwrap();

    // INSERT
    run("INSERT INTO t1 VALUES (1, 'Alice')", &mut cache, &mut tsm).unwrap();
    run("INSERT INTO t1 VALUES (2, 'Bob')", &mut cache, &mut tsm).unwrap();

    // SELECT *
    let rs = run("SELECT * FROM t1", &mut cache, &mut tsm).unwrap();
    assert_eq!(rs.rows.len(), 2);
    assert_eq!(rs.columns, vec!["ID", "NAME"]);
}

#[test]
fn create_table_insert_update_delete_select() {
    let (mut cache, mut tsm, _db) = open_db("crud");

    run("CREATE TABLE items (id INTEGER NOT NULL, qty INTEGER)", &mut cache, &mut tsm).unwrap();
    run("INSERT INTO items VALUES (1, 10)", &mut cache, &mut tsm).unwrap();
    run("INSERT INTO items VALUES (2, 20)", &mut cache, &mut tsm).unwrap();
    run("INSERT INTO items VALUES (3, 30)", &mut cache, &mut tsm).unwrap();

    // UPDATE
    run("UPDATE items SET qty = 99 WHERE id = 2", &mut cache, &mut tsm).unwrap();

    // DELETE
    run("DELETE FROM items WHERE id = 3", &mut cache, &mut tsm).unwrap();

    // Verify
    let rs = run("SELECT * FROM items", &mut cache, &mut tsm).unwrap();
    assert_eq!(rs.rows.len(), 2);

    // Row with id=2 should have qty=99
    let row2 = rs.rows.iter().find(|r| r[0] == Value::Integer(2)).unwrap();
    assert_eq!(row2[1], Value::Integer(99));
}

// ---------------------------------------------------------------------------
// Tests: Persistence across restart
// ---------------------------------------------------------------------------

#[test]
fn data_persists_across_restart() {
    let (mut cache, mut tsm, db) = open_db("persist");

    // Create table and insert data
    run("CREATE TABLE persist_t (id INTEGER NOT NULL, val VARCHAR(20))", &mut cache, &mut tsm).unwrap();
    run("INSERT INTO persist_t VALUES (1, 'hello')", &mut cache, &mut tsm).unwrap();
    run("INSERT INTO persist_t VALUES (2, 'world')", &mut cache, &mut tsm).unwrap();

    // Flush to disk
    tsm.flush_all().unwrap();
    drop(tsm);
    drop(cache);

    // --- Restart: reload from disk ---
    let (mut cache2, mut tsm2) = reload_db(&db);

    // The user table should still exist in the catalog
    assert!(cache2.get_table("PUBLIC", "PERSIST_T").is_some());

    // Data should still be there
    let rs = run("SELECT * FROM persist_t", &mut cache2, &mut tsm2).unwrap();
    assert_eq!(rs.rows.len(), 2);
}

// ---------------------------------------------------------------------------
// Tests: Schema-qualified access
// ---------------------------------------------------------------------------

#[test]
fn schema_qualified_create_and_select() {
    let (mut cache, mut tsm, _db) = open_db("schema_q");

    // Create in a custom schema (auto-creates the schema)
    run("CREATE TABLE myschema.t1 (x INTEGER)", &mut cache, &mut tsm).unwrap();

    // Should be accessible schema-qualified
    let rs = run("SELECT * FROM myschema.t1", &mut cache, &mut tsm).unwrap();
    assert_eq!(rs.rows.len(), 0);
    assert_eq!(rs.columns, vec!["X"]);

    // System schema should be protected
    let err = run("CREATE TABLE RQSYS.bad (x INTEGER)", &mut cache, &mut tsm);
    assert!(err.is_err());
}

// ---------------------------------------------------------------------------
// Tests: Text mode configuration
// ---------------------------------------------------------------------------

#[test]
fn text_mode_bootstrap_and_load_catalog() {
    let db = TempDb::new("text_mode");
    let cfg = DbConfig {
        text_mode: true,
        ..DbConfig::default()
    };
    bootstrap::bootstrap(db.path(), &cfg).unwrap();
    let catalog = loader::load_catalog(db.path(), &cfg).unwrap();
    let cache = CatalogCache::new(catalog, cfg);

    // Text mode writes TSV files (for inspection); verify the catalog loaded.
    assert!(cache.get_tablespace_by_id(1).is_some()); // SYSTBSP
    assert!(cache.has_schema("PUBLIC"));
    assert!(cache.get_table(&cache.config().sys_schema, "SYSTABLES").is_some());
}

// ---------------------------------------------------------------------------
// Tests: Multi-row insert
// ---------------------------------------------------------------------------

#[test]
fn multi_row_insert() {
    let (mut cache, mut tsm, _db) = open_db("multi_ins");

    run("CREATE TABLE nums (n INTEGER NOT NULL)", &mut cache, &mut tsm).unwrap();
    run("INSERT INTO nums VALUES (1), (2), (3), (4), (5)", &mut cache, &mut tsm).unwrap();

    let rs = run("SELECT * FROM nums", &mut cache, &mut tsm).unwrap();
    assert_eq!(rs.rows.len(), 5);
}

// ---------------------------------------------------------------------------
// Tests: open_database / create_database helpers (connect-to flow)
// ---------------------------------------------------------------------------

/// Helper to get a temp base directory for connect/create tests.
fn temp_base_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("rqdb_connect_{name}"));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn connect_to_existing_database() {
    let base = temp_base_dir("connect_ok");

    // Bootstrap a database at base/TESTCONN
    let db_dir = base.join("TESTCONN");
    std::fs::create_dir_all(&db_dir).unwrap();
    let cfg = DbConfig::default();
    bootstrap::bootstrap(&db_dir, &cfg).unwrap();

    // open_database should succeed
    let state = rqdb::open_database(&base, "TESTCONN");
    assert!(state.is_ok(), "expected Ok, got: {:?}", state.err());
    let mut state = state.unwrap();
    assert_eq!(state.name, "TESTCONN");

    // Should be able to query catalog tables
    let stmts = rqdb::sql::parser::parse("SELECT * FROM SYSTABLES").unwrap();
    let rs = rqdb::sql::executor::execute(&stmts[0], &mut state.cache, &mut state.tsm);
    assert!(rs.is_ok());

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn connect_to_nonexistent_errors() {
    let base = temp_base_dir("connect_missing");

    let result = rqdb::open_database(&base, "NOSUCHDB");
    match result {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("not found"),
                "expected 'not found' in error, got: {msg}"
            );
        }
        Ok(_) => panic!("expected error for nonexistent database"),
    }

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn create_database_new() {
    let base = temp_base_dir("create_new");

    let result = rqdb::create_database(&base, "FRESHDB", false);
    assert!(result.is_ok(), "expected Ok, got: {:?}", result.err());

    // SQLDBCONF should exist
    assert!(base.join("FRESHDB").join("admin/SQLDBCONF").exists());

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn create_database_already_exists_errors() {
    let base = temp_base_dir("create_dup");

    // Create the database once
    rqdb::create_database(&base, "DUPDB", false).unwrap();

    // Second create should fail
    let result = rqdb::create_database(&base, "DUPDB", false);
    match result {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("already exists"),
                "expected 'already exists' in error, got: {msg}"
            );
        }
        Ok(_) => panic!("expected error for duplicate database"),
    }

    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Tests: disconnect flow
// ---------------------------------------------------------------------------

#[test]
fn disconnect_flushes_and_persists_data() {
    let base = temp_base_dir("disconnect_ok");

    // Create a database, insert data, then disconnect (flush+drop) and reconnect.
    let mut state = rqdb::create_database(&base, "DISCDB", false).unwrap();

    let stmts = rqdb::sql::parser::parse(
        "CREATE TABLE disc_t (id INTEGER NOT NULL, val VARCHAR(20))"
    ).unwrap();
    rqdb::sql::executor::execute(&stmts[0], &mut state.cache, &mut state.tsm).unwrap();

    let stmts = rqdb::sql::parser::parse("INSERT INTO disc_t VALUES (1, 'before')").unwrap();
    rqdb::sql::executor::execute(&stmts[0], &mut state.cache, &mut state.tsm).unwrap();

    // Simulate DISCONNECT: flush and drop
    state.tsm.flush_all().unwrap();
    drop(state);

    // Reconnect — data should still be there
    let mut state2 = rqdb::open_database(&base, "DISCDB").unwrap();
    let stmts = rqdb::sql::parser::parse("SELECT * FROM disc_t").unwrap();
    let rs = rqdb::sql::executor::execute(&stmts[0], &mut state2.cache, &mut state2.tsm).unwrap();
    assert_eq!(rs.rows.len(), 1, "expected 1 row after disconnect+reconnect");

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn disconnect_then_connect_different_db() {
    let base = temp_base_dir("disconnect_switch");

    // Create two databases
    let mut db1 = rqdb::create_database(&base, "DB1", false).unwrap();
    let stmts = rqdb::sql::parser::parse("CREATE TABLE t1 (x INTEGER)").unwrap();
    rqdb::sql::executor::execute(&stmts[0], &mut db1.cache, &mut db1.tsm).unwrap();
    db1.tsm.flush_all().unwrap();
    drop(db1);

    let mut db2 = rqdb::create_database(&base, "DB2", false).unwrap();
    let stmts = rqdb::sql::parser::parse("CREATE TABLE t2 (y INTEGER)").unwrap();
    rqdb::sql::executor::execute(&stmts[0], &mut db2.cache, &mut db2.tsm).unwrap();
    db2.tsm.flush_all().unwrap();
    drop(db2);

    // Connect to DB1, verify its table
    let state = rqdb::open_database(&base, "DB1").unwrap();
    assert_eq!(state.name, "DB1");
    assert!(state.cache.get_table("PUBLIC", "T1").is_some());
    drop(state);

    // Connect to DB2, verify isolation
    let state2 = rqdb::open_database(&base, "DB2").unwrap();
    assert_eq!(state2.name, "DB2");
    assert!(state2.cache.get_table("PUBLIC", "T2").is_some());
    assert!(state2.cache.get_table("PUBLIC", "T1").is_none());

    let _ = std::fs::remove_dir_all(&base);
}
