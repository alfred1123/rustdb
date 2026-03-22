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
use rqdb::catalog::row::MIN_COLUMN_BYTES;
use rqdb::sql::{executor, parser};
use rqdb::sql::types::Value;
use rqdb::storage::page::PAGE_HEADER_SIZE;
use rqdb::storage::tablespace::TablespaceManager;
use rqdb::storage::tuple::TUPLE_HEADER_SIZE;

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
                msg.contains("3D000"),
                "expected SQLSTATE 3D000 (DatabaseNotFound), got: {msg}"
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

    // Second create should fail with SQLSTATE 42P04
    let result = rqdb::create_database(&base, "DUPDB", false);
    match result {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("42P04"),
                "expected SQLSTATE 42P04 (DatabaseAlreadyExists), got: {msg}"
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

// ---------------------------------------------------------------------------
// Tests: SQLSTATE error codes — end-to-end validation
// ---------------------------------------------------------------------------

/// Assert that a SQL execution result is an error containing the given SQLSTATE code.
fn assert_sqlstate_integ(result: rqdb::error::Result<rqdb::sql::types::ResultSet>, code: &str) {
    match result {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains(code),
                "expected SQLSTATE {code} in error, got: {msg}"
            );
        }
        Ok(_) => panic!("expected SQLSTATE {code} error, got Ok"),
    }
}

// 42S02 — TableNotFound

#[test]
fn sqlstate_table_not_found_select() {
    let (mut cache, mut tsm, _db) = open_db("integ_42s02_sel");
    assert_sqlstate_integ(
        run("SELECT * FROM ghost_table", &mut cache, &mut tsm),
        "42S02",
    );
}

#[test]
fn sqlstate_table_not_found_insert() {
    let (mut cache, mut tsm, _db) = open_db("integ_42s02_ins");
    assert_sqlstate_integ(
        run("INSERT INTO ghost_table VALUES (1)", &mut cache, &mut tsm),
        "42S02",
    );
}

#[test]
fn sqlstate_table_not_found_delete() {
    let (mut cache, mut tsm, _db) = open_db("integ_42s02_del");
    assert_sqlstate_integ(
        run("DELETE FROM ghost_table WHERE id = 1", &mut cache, &mut tsm),
        "42S02",
    );
}

#[test]
fn sqlstate_table_not_found_update() {
    let (mut cache, mut tsm, _db) = open_db("integ_42s02_upd");
    assert_sqlstate_integ(
        run("UPDATE ghost_table SET x = 1", &mut cache, &mut tsm),
        "42S02",
    );
}

#[test]
fn sqlstate_table_not_found_drop() {
    let (mut cache, mut tsm, _db) = open_db("integ_42s02_drop");
    assert_sqlstate_integ(
        run("DROP TABLE ghost_table", &mut cache, &mut tsm),
        "42S02",
    );
}

// 42S22 — ColumnNotFound

#[test]
fn sqlstate_column_not_found_in_select() {
    let (mut cache, mut tsm, _db) = open_db("integ_42s22_sel");
    run("CREATE TABLE t (id INTEGER, name VARCHAR(20))", &mut cache, &mut tsm).unwrap();
    assert_sqlstate_integ(
        run("SELECT bogus FROM t", &mut cache, &mut tsm),
        "42S22",
    );
}

#[test]
fn sqlstate_column_not_found_in_where() {
    let (mut cache, mut tsm, _db) = open_db("integ_42s22_whr");
    run("CREATE TABLE t (id INTEGER)", &mut cache, &mut tsm).unwrap();
    run("INSERT INTO t VALUES (1)", &mut cache, &mut tsm).unwrap();
    assert_sqlstate_integ(
        run("SELECT * FROM t WHERE ghost = 1", &mut cache, &mut tsm),
        "42S22",
    );
}

#[test]
fn sqlstate_column_not_found_in_update_set() {
    let (mut cache, mut tsm, _db) = open_db("integ_42s22_upd");
    run("CREATE TABLE t (id INTEGER)", &mut cache, &mut tsm).unwrap();
    run("INSERT INTO t VALUES (1)", &mut cache, &mut tsm).unwrap();
    assert_sqlstate_integ(
        run("UPDATE t SET ghost = 99 WHERE id = 1", &mut cache, &mut tsm),
        "42S22",
    );
}

// 42S01 — TableAlreadyExists

#[test]
fn sqlstate_table_already_exists() {
    let (mut cache, mut tsm, _db) = open_db("integ_42s01");
    run("CREATE TABLE dup (id INTEGER)", &mut cache, &mut tsm).unwrap();
    assert_sqlstate_integ(
        run("CREATE TABLE dup (x VARCHAR(10))", &mut cache, &mut tsm),
        "42S01",
    );
}

// 42711 — DuplicateColumnName

#[test]
fn sqlstate_duplicate_column_name() {
    let (mut cache, mut tsm, _db) = open_db("integ_42711");
    assert_sqlstate_integ(
        run("CREATE TABLE bad (id INTEGER, name VARCHAR(10), id SMALLINT)", &mut cache, &mut tsm),
        "42711",
    );
}

// 42611 — InvalidColumnLength

#[test]
fn sqlstate_invalid_column_length_zero() {
    let (mut cache, mut tsm, _db) = open_db("integ_42611_z");
    assert_sqlstate_integ(
        run("CREATE TABLE bad (name CHAR(0))", &mut cache, &mut tsm),
        "42611",
    );
}

#[test]
fn sqlstate_invalid_column_length_too_large() {
    let (mut cache, mut tsm, _db) = open_db("integ_42611_big");
    assert_sqlstate_integ(
        run("CREATE TABLE bad (data VARCHAR(40000))", &mut cache, &mut tsm),
        "42611",
    );
}

// 42508 — SystemSchemaViolation

#[test]
fn sqlstate_system_schema_create_rejected() {
    let (mut cache, mut tsm, _db) = open_db("integ_42508_cr");
    assert_sqlstate_integ(
        run("CREATE TABLE RQSYS.forbidden (id INTEGER)", &mut cache, &mut tsm),
        "42508",
    );
}

#[test]
fn sqlstate_system_schema_drop_rejected() {
    let (mut cache, mut tsm, _db) = open_db("integ_42508_dr");
    assert_sqlstate_integ(
        run("DROP TABLE RQSYS.SYSTABLES", &mut cache, &mut tsm),
        "42508",
    );
}

// 21S01 — InsertValueListMismatch

#[test]
fn sqlstate_insert_value_list_mismatch_too_few() {
    let (mut cache, mut tsm, _db) = open_db("integ_21s01_few");
    run("CREATE TABLE t (id INTEGER, name VARCHAR(20), qty INTEGER)", &mut cache, &mut tsm).unwrap();
    assert_sqlstate_integ(
        run("INSERT INTO t VALUES (1, 'x')", &mut cache, &mut tsm),
        "21S01",
    );
}

#[test]
fn sqlstate_insert_value_list_mismatch_too_many() {
    let (mut cache, mut tsm, _db) = open_db("integ_21s01_many");
    run("CREATE TABLE t (id INTEGER)", &mut cache, &mut tsm).unwrap();
    assert_sqlstate_integ(
        run("INSERT INTO t VALUES (1, 'extra', 99)", &mut cache, &mut tsm),
        "21S01",
    );
}

// 23502 — NotNullViolation

#[test]
fn sqlstate_not_null_violation() {
    let (mut cache, mut tsm, _db) = open_db("integ_23502");
    run("CREATE TABLE t (id INTEGER NOT NULL, name VARCHAR(20))", &mut cache, &mut tsm).unwrap();
    assert_sqlstate_integ(
        run("INSERT INTO t VALUES (NULL, 'Alice')", &mut cache, &mut tsm),
        "23502",
    );
}

// 22005 — AssignmentError (type mismatch)

#[test]
fn sqlstate_assignment_error_string_to_int() {
    let (mut cache, mut tsm, _db) = open_db("integ_22005");
    run("CREATE TABLE t (id INTEGER, name VARCHAR(20))", &mut cache, &mut tsm).unwrap();
    assert_sqlstate_integ(
        run("INSERT INTO t VALUES ('not_a_number', 'Alice')", &mut cache, &mut tsm),
        "22005",
    );
}

// 0A000 — FeatureNotSupported

#[test]
fn sqlstate_feature_not_supported_join() {
    let (mut cache, mut tsm, _db) = open_db("integ_0a000_join");
    run("CREATE TABLE a (id INTEGER)", &mut cache, &mut tsm).unwrap();
    run("CREATE TABLE b (id INTEGER)", &mut cache, &mut tsm).unwrap();
    assert_sqlstate_integ(
        run("SELECT * FROM a JOIN b ON a.id = b.id", &mut cache, &mut tsm),
        "0A000",
    );
}

#[test]
fn sqlstate_feature_not_supported_unsupported_stmt() {
    let (mut cache, mut tsm, _db) = open_db("integ_0a000_stmt");
    assert_sqlstate_integ(
        run("ALTER TABLE SYSTABLES ADD COLUMN x INTEGER", &mut cache, &mut tsm),
        "0A000",
    );
}

// 42000 — SyntaxError (semantic, not parse)

#[test]
fn sqlstate_syntax_error_no_from() {
    let (mut cache, mut tsm, _db) = open_db("integ_42000_nofrom");
    assert_sqlstate_integ(
        run("SELECT * FROM a, b", &mut cache, &mut tsm),
        "42000",
    );
}

// 22000 — DataException

#[test]
fn sqlstate_data_exception_unsupported_literal() {
    let (mut cache, mut tsm, _db) = open_db("integ_22000");
    run("CREATE TABLE t (id INTEGER)", &mut cache, &mut tsm).unwrap();
    assert_sqlstate_integ(
        run("INSERT INTO t VALUES (X'DEADBEEF')", &mut cache, &mut tsm),
        "22000",
    );
}

// DROP TABLE IF EXISTS — should NOT error

#[test]
fn drop_table_if_exists_no_error() {
    let (mut cache, mut tsm, _db) = open_db("integ_dt_ifexists");
    let rs = run("DROP TABLE IF EXISTS nonexistent", &mut cache, &mut tsm).unwrap();
    assert!(rs.rows[0][0].to_string().contains("skipped"));
}

// DROP TABLE full lifecycle (integration)

#[test]
fn drop_table_end_to_end() {
    let (mut cache, mut tsm, _db) = open_db("integ_dt_e2e");

    run("CREATE TABLE dropme (id INTEGER NOT NULL, val VARCHAR(30))", &mut cache, &mut tsm).unwrap();
    run("INSERT INTO dropme VALUES (1, 'alpha')", &mut cache, &mut tsm).unwrap();
    run("INSERT INTO dropme VALUES (2, 'beta')", &mut cache, &mut tsm).unwrap();

    let rs = run("SELECT * FROM dropme", &mut cache, &mut tsm).unwrap();
    assert_eq!(rs.rows.len(), 2);

    run("DROP TABLE dropme", &mut cache, &mut tsm).unwrap();

    // Should not be queryable
    assert_sqlstate_integ(
        run("SELECT * FROM dropme", &mut cache, &mut tsm),
        "42S02",
    );

    // Should not appear in catalog
    let rs = run("SELECT * FROM SYSTABLES WHERE name = 'DROPME'", &mut cache, &mut tsm).unwrap();
    assert_eq!(rs.rows.len(), 0);

    let rs = run("SELECT * FROM SYSCOLUMNS WHERE tabname = 'DROPME'", &mut cache, &mut tsm).unwrap();
    assert_eq!(rs.rows.len(), 0);

    // Should be re-creatable
    run("CREATE TABLE dropme (x VARCHAR(10))", &mut cache, &mut tsm).unwrap();
    run("INSERT INTO dropme VALUES ('reborn')", &mut cache, &mut tsm).unwrap();
    let rs = run("SELECT * FROM dropme", &mut cache, &mut tsm).unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.rows[0][0].to_string(), "reborn");
}

// ---------------------------------------------------------------------------
// Tests: Row size and column count limits (MVCC tuple header awareness)
// ---------------------------------------------------------------------------

const SLOT_ENTRY_SIZE: usize = 4; // u16 offset + u16 length

/// Compute the max user-data payload for a page, accounting for the 16-byte
/// MVCC tuple header.  This mirrors the formula in executor.rs.
fn max_user_payload(page_size: usize) -> usize {
    page_size - PAGE_HEADER_SIZE - SLOT_ENTRY_SIZE - TUPLE_HEADER_SIZE
}

#[test]
fn row_size_limit_rejects_oversized_table() {
    let (mut cache, mut tsm, _db) = open_db("integ_row_too_big");

    // Default page size is 4096.
    // max_user_payload = 4096 - 24 - 4 - 16 = 4052
    // INTEGER = 8 (prefix) + 4 (data) = 12
    // VARCHAR(4050) = 8 + 4050 = 4058
    // Total = 12 + 4058 = 4070 > 4052  →  should be rejected
    let err = run(
        "CREATE TABLE too_big (id INTEGER, data VARCHAR(4050))",
        &mut cache, &mut tsm,
    );
    assert!(err.is_err(), "expected RowTooLarge error");
    let msg = err.unwrap_err().to_string();
    assert!(
        msg.contains("54010"),
        "expected SQLSTATE 54010 (RowTooLarge), got: {msg}"
    );
}

#[test]
fn row_size_limit_accepts_boundary_table() {
    let (mut cache, mut tsm, _db) = open_db("integ_row_just_fit");

    // max_user_payload = 4052
    // INTEGER = 12,  VARCHAR(4032) = 8 + 4032 = 4040  →  total = 4052  (exact fit)
    run(
        "CREATE TABLE just_fit (id INTEGER, data VARCHAR(4032))",
        &mut cache, &mut tsm,
    ).expect("table at exact row-size boundary should be created");

    // Verify the table exists in catalog
    assert!(cache.get_table("PUBLIC", "JUST_FIT").is_some());
}

#[test]
fn row_size_limit_boundary_insert_succeeds() {
    let (mut cache, mut tsm, _db) = open_db("integ_row_bnd_ins");

    // Create a table at the row-size boundary
    run(
        "CREATE TABLE bnd (id INTEGER, data VARCHAR(4032))",
        &mut cache, &mut tsm,
    ).unwrap();

    // Insert a row with data shorter than the max — must succeed
    run(
        "INSERT INTO bnd VALUES (1, 'hello')",
        &mut cache, &mut tsm,
    ).expect("insert into boundary table should succeed");

    // Verify the row is readable
    let rs = run("SELECT * FROM bnd", &mut cache, &mut tsm).unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.rows[0][0], Value::Integer(1));
}

#[test]
fn column_count_limit_rejects_excess() {
    let (mut cache, mut tsm, _db) = open_db("integ_col_excess");

    let max_cols = max_user_payload(4096) / MIN_COLUMN_BYTES; // 4052 / 9 = 450

    // One more than the limit should be rejected
    let over = max_cols + 1;
    let cols: Vec<String> = (0..over).map(|i| format!("c{i} CHAR(1)")).collect();
    let sql = format!("CREATE TABLE too_wide ({})", cols.join(", "));
    let err = run(&sql, &mut cache, &mut tsm);
    assert!(err.is_err(), "expected TooManyColumns error for {over} columns");
    let msg = err.unwrap_err().to_string();
    assert!(
        msg.contains("54011"),
        "expected SQLSTATE 54011 (TooManyColumns), got: {msg}"
    );
}

#[test]
fn column_count_limit_accepts_boundary() {
    let (mut cache, mut tsm, _db) = open_db("integ_col_bnd");

    let max_cols = max_user_payload(4096) / MIN_COLUMN_BYTES; // 450

    // Exactly at the limit should succeed
    let cols: Vec<String> = (0..max_cols).map(|i| format!("c{i} CHAR(1)")).collect();
    let sql = format!("CREATE TABLE wide_ok ({})", cols.join(", "));
    run(&sql, &mut cache, &mut tsm)
        .unwrap_or_else(|e| panic!("table with {max_cols} columns should be created: {e}"));

    assert!(cache.get_table("PUBLIC", "WIDE_OK").is_some());
}

#[test]
fn column_count_boundary_insert_and_select() {
    let (mut cache, mut tsm, _db) = open_db("integ_col_bnd_ins");

    let max_cols = max_user_payload(4096) / MIN_COLUMN_BYTES; // 450

    // Create a table at the column-count boundary
    let cols: Vec<String> = (0..max_cols).map(|i| format!("c{i} CHAR(1)")).collect();
    let sql = format!("CREATE TABLE wide ({})", cols.join(", "));
    run(&sql, &mut cache, &mut tsm).unwrap();

    // Insert a row with all columns set to 'A'
    let vals: Vec<&str> = (0..max_cols).map(|_| "'A'").collect();
    let sql = format!("INSERT INTO wide VALUES ({})", vals.join(", "));
    run(&sql, &mut cache, &mut tsm)
        .expect("insert into max-column table should succeed");

    // Read it back
    let rs = run("SELECT * FROM wide", &mut cache, &mut tsm).unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.columns.len(), max_cols);
}

#[test]
fn row_size_off_by_one_above_boundary_rejected() {
    let (mut cache, mut tsm, _db) = open_db("integ_row_off1");

    // max_user_payload = 4052.  INTEGER(12) + VARCHAR(4033)(8+4033=4041) = 4053 > 4052
    let err = run(
        "CREATE TABLE off1 (id INTEGER, data VARCHAR(4033))",
        &mut cache, &mut tsm,
    );
    assert!(err.is_err(), "1 byte over the limit should be rejected");
    assert!(err.unwrap_err().to_string().contains("54010"));
}
