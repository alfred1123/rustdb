use std::collections::HashMap;

use crate::catalog::config::DbConfig;
use crate::catalog::types::*;
use crate::sql::types::Value;

/// Pre-materialized data for a single catalog table, with O(1) column lookup.
pub struct CachedTable {
    /// Ordered column names.
    pub column_names: Vec<String>,
    /// Column name → positional index. O(1) lookup.
    pub column_index: HashMap<String, usize>,
    /// All rows pre-converted to `Value` vecs.
    pub rows: Vec<Vec<Value>>,
}

/// In-memory catalog cache. Stays resident for the lifetime of the database.
///
/// Provides O(1) lookups by name/ID for tables, columns, tablespaces, and
/// schemas. Pre-materializes catalog rows so the executor never needs to
/// convert structs to `Value` at query time.
pub struct CatalogCache {
    /// The underlying typed catalog (for direct struct access).
    catalog: Catalog,
    /// Database configuration (SQLDBCONF).
    config: DbConfig,
    /// (SCHEMA, TABLE_NAME) → pre-materialized table data. O(1) lookup.
    tables_data: HashMap<(String, String), CachedTable>,
    /// (SCHEMA, TABLE_NAME) → index into `catalog.tables`.
    table_idx: HashMap<(String, String), usize>,
    /// TBSPACEID → index into `catalog.tablespaces`.
    tablespace_by_id: HashMap<i32, usize>,
    /// SCHEMA_NAME → index into `catalog.schemas`.
    schema_idx: HashMap<String, usize>,
    /// (SCHEMA, TABLE_NAME) → sorted columns for that table.
    columns_by_table: HashMap<(String, String), Vec<Column>>,
    /// (SCHEMA, TABLE_NAME) → (column_names, column_name→index). Built once.
    column_meta: HashMap<(String, String), (Vec<String>, HashMap<String, usize>)>,
}

impl CatalogCache {
    /// Build the cache from a loaded catalog. Indexes and pre-materializes
    /// all catalog data for O(1) access.
    pub fn new(catalog: Catalog, config: DbConfig) -> Self {
        let table_idx: HashMap<(String, String), usize> = catalog
            .tables
            .iter()
            .enumerate()
            .map(|(i, t)| ((t.schemaname.clone(), t.name.clone()), i))
            .collect();

        let tablespace_by_id: HashMap<i32, usize> = catalog
            .tablespaces
            .iter()
            .enumerate()
            .map(|(i, ts)| (ts.tbspaceid, i))
            .collect();

        let schema_idx: HashMap<String, usize> = catalog
            .schemas
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name.clone(), i))
            .collect();

        // Group columns by (schema, table), sorted by ordinal.
        let mut columns_by_table: HashMap<(String, String), Vec<Column>> = HashMap::new();
        for col in &catalog.columns {
            columns_by_table
                .entry((col.schemaname.clone(), col.tabname.clone()))
                .or_default()
                .push(col.clone());
        }
        for cols in columns_by_table.values_mut() {
            cols.sort_by_key(|c| c.ordinal);
        }

        let tables_data = Self::materialize_tables(&catalog, &config.sys_schema, &columns_by_table);

        // Pre-build column name vectors and name→index maps for every table.
        let column_meta: HashMap<(String, String), (Vec<String>, HashMap<String, usize>)> =
            columns_by_table
                .iter()
                .map(|(key, cols)| {
                    let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
                    let index = Self::build_column_index(&names);
                    (key.clone(), (names, index))
                })
                .collect();

        Self {
            catalog,
            config,
            tables_data,
            table_idx,
            tablespace_by_id,
            schema_idx,
            columns_by_table,
            column_meta,
        }
    }

    /// O(1) table metadata lookup by (schema, name).
    pub fn get_table(&self, schema: &str, name: &str) -> Option<&Table> {
        self.table_idx
            .get(&(schema.to_string(), name.to_string()))
            .map(|&i| &self.catalog.tables[i])
    }

    /// Database configuration (SQLDBCONF).
    pub fn config(&self) -> &DbConfig {
        &self.config
    }

    /// Resolve the default tablespace name to its ID.
    /// Falls back to ID 2 (USERTBSP) if the configured name is not found.
    pub fn default_tablespace_id(&self) -> i16 {
        self.catalog
            .tablespaces
            .iter()
            .find(|ts| ts.tbspace == self.config.default_tablespace)
            .map(|ts| ts.tbspaceid as i16)
            .unwrap_or(2)
    }

    /// O(1) columns for a table, sorted by ordinal.
    pub fn get_columns(&self, schema: &str, table: &str) -> Option<&[Column]> {
        self.columns_by_table
            .get(&(schema.to_string(), table.to_string()))
            .map(|v| v.as_slice())
    }

    /// O(1) tablespace lookup by ID.
    pub fn get_tablespace_by_id(&self, id: i32) -> Option<&Tablespace> {
        self.tablespace_by_id
            .get(&id)
            .map(|&i| &self.catalog.tablespaces[i])
    }

    /// O(1) lookup of precomputed column names and name→index map.
    pub fn get_column_meta(
        &self,
        schema: &str,
        table: &str,
    ) -> Option<(&[String], &HashMap<String, usize>)> {
        self.column_meta
            .get(&(schema.to_string(), table.to_string()))
            .map(|(names, idx)| (names.as_slice(), idx))
    }

    /// O(1) pre-materialized table data for the executor.
    pub fn get_table_data(&self, schema: &str, table: &str) -> Option<&CachedTable> {
        self.tables_data
            .get(&(schema.to_string(), table.to_string()))
    }

    /// Raw catalog access.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Return the next available TABLEID (max existing + 1).
    pub fn next_table_id(&self) -> i32 {
        self.catalog
            .tables
            .iter()
            .map(|t| t.tableid)
            .max()
            .unwrap_or(0)
            + 1
    }

    /// Check whether a schema exists.
    pub fn has_schema(&self, name: &str) -> bool {
        self.schema_idx.contains_key(name)
    }

    /// Register a new schema in the cache (after persisting to SYSSCHEMAS).
    pub fn register_schema(&mut self, schema: Schema) {
        let idx = self.catalog.schemas.len();
        self.schema_idx.insert(schema.name.clone(), idx);
        self.catalog.schemas.push(schema);
        self.rematerialize("SYSSCHEMAS");
    }

    /// Register a newly-created table and its columns in the cache.
    ///
    /// Call this **after** the catalog rows have been persisted to
    /// SYSTABLES / SYSCOLUMNS via the tablespace manager.
    pub fn register_table(&mut self, table: Table, columns: Vec<Column>) {
        let key = (table.schemaname.clone(), table.name.clone());

        // catalog.tables
        let idx = self.catalog.tables.len();
        self.table_idx.insert(key.clone(), idx);
        self.catalog.tables.push(table);

        // columns_by_table  (already sorted by caller)
        self.columns_by_table.insert(key.clone(), columns.clone());

        // column_meta
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        let index = Self::build_column_index(&names);
        self.column_meta.insert(key.clone(), (names, index));

        // catalog.columns
        self.catalog.columns.extend(columns);

        // Re-materialize SYSTABLES and SYSCOLUMNS so SELECTs see them.
        self.rematerialize("SYSTABLES");
        self.rematerialize("SYSCOLUMNS");
    }

    // ── Internal: materialize catalog tables generically ──

    /// Build CachedTable entries for every table in the system schema.
    /// Column names come from SYSCOLUMNS metadata (via `columns_by_table`),
    /// not hardcoded lists. Row data comes from `catalog_rows` dispatch.
    fn materialize_tables(
        catalog: &Catalog,
        sys_schema: &str,
        columns_by_table: &HashMap<(String, String), Vec<Column>>,
    ) -> HashMap<(String, String), CachedTable> {
        let mut map = HashMap::new();
        for table in &catalog.tables {
            if table.schemaname == sys_schema {
                let key = (sys_schema.to_string(), table.name.clone());
                if let Some(cols) = columns_by_table.get(&key) {
                    let column_names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
                    let column_index = Self::build_column_index(&column_names);
                    let rows = Self::catalog_rows(catalog, &table.name);
                    map.insert(key, CachedTable { column_names, column_index, rows });
                }
            }
        }
        map
    }

    /// Re-materialize a single system catalog table after DDL mutations.
    fn rematerialize(&mut self, table_name: &str) {
        let key = (self.config.sys_schema.clone(), table_name.to_string());
        let column_names: Vec<String> = match self.columns_by_table.get(&key) {
            Some(cols) => cols.iter().map(|c| c.name.clone()).collect(),
            None => return,
        };
        let column_index = Self::build_column_index(&column_names);
        let rows = Self::catalog_rows(&self.catalog, table_name);
        self.tables_data.insert(key, CachedTable { column_names, column_index, rows });
    }

    /// Convert catalog structs to Value rows for a given table name.
    ///
    /// Single dispatch point for struct → Value conversion.  When a new
    /// system catalog table is added, add one match arm here.
    fn catalog_rows(catalog: &Catalog, table_name: &str) -> Vec<Vec<Value>> {
        match table_name {
            "SYSTABLESPACES" => catalog.tablespaces.iter().map(|ts| vec![
                Value::Integer(ts.tbspaceid),
                Value::Str(ts.tbspace.clone()),
                Value::Str(ts.tbspacetype.clone()),
                Value::Str(ts.datatype.clone()),
                Value::Integer(ts.pagesize),
                Value::Str(ts.state.clone()),
                Value::Integer(ts.bufferpoolid),
            ]).collect(),
            "SYSSCHEMAS" => catalog.schemas.iter().map(|s| vec![
                Value::Str(s.name.clone()),
            ]).collect(),
            "SYSTABLES" => catalog.tables.iter().map(|t| vec![
                Value::Integer(t.tableid),
                Value::Str(t.name.clone()),
                Value::Str(t.schemaname.clone()),
                Value::SmallInt(t.tbspaceid),
                Value::SmallInt(t.colcount),
            ]).collect(),
            "SYSCOLUMNS" => catalog.columns.iter().map(|c| vec![
                Value::Str(c.name.clone()),
                Value::Str(c.tabname.clone()),
                Value::Str(c.schemaname.clone()),
                Value::SmallInt(c.ordinal),
                Value::Str(c.typename.clone()),
                Value::Bool(c.nullable),
            ]).collect(),
            "SYSBUFFERPOOLS" => catalog.bufferpools.iter().map(|bp| vec![
                Value::Integer(bp.bpid),
                Value::Str(bp.bpname.clone()),
                Value::Integer(bp.pagesize),
                Value::Integer(bp.npages),
            ]).collect(),
            _ => vec![],
        }
    }

    fn build_column_index(names: &[String]) -> HashMap<String, usize> {
        names.iter().enumerate().map(|(i, n)| (n.clone(), i)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_catalog() -> Catalog {
        Catalog {
            tablespaces: vec![
                Tablespace {
                    tbspaceid: 1,
                    tbspace: "SYSTBSP".into(),
                    tbspacetype: "S".into(),
                    datatype: "A".into(),
                    pagesize: 4096,
                    state: "N".into(),
                    bufferpoolid: 1,
                },
                Tablespace {
                    tbspaceid: 2,
                    tbspace: "USERTBSP".into(),
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
                    tableid: 1,
                    name: "SYSTABLESPACES".into(),
                    schemaname: "RQSYS".into(),
                    tbspaceid: 1,
                    colcount: 7,
                },
                Table {
                    tableid: 2,
                    name: "SYSTABLES".into(),
                    schemaname: "RQSYS".into(),
                    tbspaceid: 1,
                    colcount: 5,
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
                Column {
                    name: "TBSPACETYPE".into(),
                    tabname: "SYSTABLESPACES".into(),
                    schemaname: "RQSYS".into(),
                    ordinal: 2,
                    typename: "CHAR(1)".into(),
                    nullable: false,
                },
                Column {
                    name: "DATATYPE".into(),
                    tabname: "SYSTABLESPACES".into(),
                    schemaname: "RQSYS".into(),
                    ordinal: 3,
                    typename: "CHAR(1)".into(),
                    nullable: false,
                },
                Column {
                    name: "PAGESIZE".into(),
                    tabname: "SYSTABLESPACES".into(),
                    schemaname: "RQSYS".into(),
                    ordinal: 4,
                    typename: "INTEGER".into(),
                    nullable: false,
                },
                Column {
                    name: "STATE".into(),
                    tabname: "SYSTABLESPACES".into(),
                    schemaname: "RQSYS".into(),
                    ordinal: 5,
                    typename: "CHAR(1)".into(),
                    nullable: false,
                },
                Column {
                    name: "BUFFERPOOLID".into(),
                    tabname: "SYSTABLESPACES".into(),
                    schemaname: "RQSYS".into(),
                    ordinal: 6,
                    typename: "INTEGER".into(),
                    nullable: false,
                },
                Column {
                    name: "TABLEID".into(),
                    tabname: "SYSTABLES".into(),
                    schemaname: "RQSYS".into(),
                    ordinal: 0,
                    typename: "INTEGER".into(),
                    nullable: false,
                },
                Column {
                    name: "NAME".into(),
                    tabname: "SYSTABLES".into(),
                    schemaname: "RQSYS".into(),
                    ordinal: 1,
                    typename: "VARCHAR(128)".into(),
                    nullable: false,
                },
                Column {
                    name: "SCHEMANAME".into(),
                    tabname: "SYSTABLES".into(),
                    schemaname: "RQSYS".into(),
                    ordinal: 2,
                    typename: "VARCHAR(128)".into(),
                    nullable: false,
                },
                Column {
                    name: "TBSPACEID".into(),
                    tabname: "SYSTABLES".into(),
                    schemaname: "RQSYS".into(),
                    ordinal: 3,
                    typename: "SMALLINT".into(),
                    nullable: false,
                },
                Column {
                    name: "COLCOUNT".into(),
                    tabname: "SYSTABLES".into(),
                    schemaname: "RQSYS".into(),
                    ordinal: 4,
                    typename: "SMALLINT".into(),
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
    fn lookup_table_by_name() {
        let cache = CatalogCache::new(test_catalog(), DbConfig::default());
        let t = cache.get_table("RQSYS", "SYSTABLESPACES").unwrap();
        assert_eq!(t.name, "SYSTABLESPACES");
        assert_eq!(t.colcount, 7);
        assert!(cache.get_table("RQSYS", "NONEXISTENT").is_none());
    }

    #[test]
    fn lookup_tablespace_by_id() {
        let cache = CatalogCache::new(test_catalog(), DbConfig::default());
        let ts = cache.get_tablespace_by_id(1).unwrap();
        assert_eq!(ts.tbspace, "SYSTBSP");
        let ts2 = cache.get_tablespace_by_id(2).unwrap();
        assert_eq!(ts2.tbspace, "USERTBSP");
        assert!(cache.get_tablespace_by_id(99).is_none());
    }

    #[test]
    fn lookup_columns_sorted() {
        let cache = CatalogCache::new(test_catalog(), DbConfig::default());
        let cols = cache.get_columns("RQSYS", "SYSTABLESPACES").unwrap();
        assert_eq!(cols.len(), 7);
        assert_eq!(cols[0].name, "TBSPACEID");
        assert_eq!(cols[1].name, "TBSPACE");
        assert_eq!(cols[6].name, "BUFFERPOOLID");
        // Ordinals are in order.
        assert!(cols[0].ordinal < cols[1].ordinal);
    }

    #[test]
    fn cached_table_data_matches() {
        let cache = CatalogCache::new(test_catalog(), DbConfig::default());
        let ct = cache.get_table_data("RQSYS", "SYSTABLESPACES").unwrap();
        assert_eq!(ct.column_names.len(), 7);
        assert_eq!(ct.rows.len(), 2);
        // O(1) column index works.
        assert_eq!(*ct.column_index.get("TBSPACEID").unwrap(), 0);
        assert_eq!(*ct.column_index.get("BUFFERPOOLID").unwrap(), 6);
    }

    #[test]
    fn cached_table_not_found() {
        let cache = CatalogCache::new(test_catalog(), DbConfig::default());
        assert!(cache.get_table_data("RQSYS", "NONEXISTENT").is_none());
    }

    #[test]
    fn schema_lookup() {
        let cache = CatalogCache::new(test_catalog(), DbConfig::default());
        assert!(cache.schema_idx.contains_key("RQSYS"));
        assert!(!cache.schema_idx.contains_key("BOGUS"));
    }
}
