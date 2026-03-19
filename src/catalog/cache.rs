use std::collections::HashMap;

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
}

impl CatalogCache {
    /// Build the cache from a loaded catalog. Indexes and pre-materializes
    /// all catalog data for O(1) access.
    pub fn new(catalog: Catalog) -> Self {
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

        let tables_data = Self::materialize_tables(&catalog);

        Self {
            catalog,
            tables_data,
            table_idx,
            tablespace_by_id,
            schema_idx,
            columns_by_table,
        }
    }

    /// O(1) table metadata lookup by (schema, name).
    pub fn get_table(&self, schema: &str, name: &str) -> Option<&Table> {
        self.table_idx
            .get(&(schema.to_string(), name.to_string()))
            .map(|&i| &self.catalog.tables[i])
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

    /// O(1) pre-materialized table data for the executor.
    pub fn get_table_data(&self, schema: &str, table: &str) -> Option<&CachedTable> {
        self.tables_data
            .get(&(schema.to_string(), table.to_string()))
    }

    /// Raw catalog access.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    // ── Internal: pre-materialize all catalog tables ──

    fn materialize_tables(catalog: &Catalog) -> HashMap<(String, String), CachedTable> {
        let schema = "RQSYS".to_string();
        let mut map = HashMap::new();

        // SYSTABLESPACES
        map.insert(
            (schema.clone(), "SYSTABLESPACES".into()),
            Self::materialize_systablespaces(catalog),
        );

        // SYSSCHEMAS
        map.insert(
            (schema.clone(), "SYSSCHEMAS".into()),
            Self::materialize_sysschemas(catalog),
        );

        // SYSTABLES
        map.insert(
            (schema.clone(), "SYSTABLES".into()),
            Self::materialize_systables(catalog),
        );

        // SYSCOLUMNS
        map.insert(
            (schema.clone(), "SYSCOLUMNS".into()),
            Self::materialize_syscolumns(catalog),
        );

        // SYSBUFFERPOOLS
        map.insert(
            (schema.clone(), "SYSBUFFERPOOLS".into()),
            Self::materialize_sysbufferpools(catalog),
        );

        map
    }

    fn build_column_index(names: &[String]) -> HashMap<String, usize> {
        names.iter().enumerate().map(|(i, n)| (n.clone(), i)).collect()
    }

    fn materialize_systablespaces(catalog: &Catalog) -> CachedTable {
        let column_names: Vec<String> = vec![
            "TBSPACEID".into(), "TBSPACE".into(), "TBSPACETYPE".into(),
            "DATATYPE".into(), "PAGESIZE".into(), "STATE".into(),
            "BUFFERPOOLID".into(),
        ];
        let rows = catalog.tablespaces.iter().map(|ts| vec![
            Value::Integer(ts.tbspaceid),
            Value::Str(ts.tbspace.clone()),
            Value::Str(ts.tbspacetype.clone()),
            Value::Str(ts.datatype.clone()),
            Value::Integer(ts.pagesize),
            Value::Str(ts.state.clone()),
            Value::Integer(ts.bufferpoolid),
        ]).collect();
        let column_index = Self::build_column_index(&column_names);
        CachedTable { column_names, column_index, rows }
    }

    fn materialize_sysschemas(catalog: &Catalog) -> CachedTable {
        let column_names: Vec<String> = vec!["NAME".into()];
        let rows = catalog.schemas.iter().map(|s| vec![
            Value::Str(s.name.clone()),
        ]).collect();
        let column_index = Self::build_column_index(&column_names);
        CachedTable { column_names, column_index, rows }
    }

    fn materialize_systables(catalog: &Catalog) -> CachedTable {
        let column_names: Vec<String> = vec![
            "NAME".into(), "SCHEMANAME".into(),
            "TBSPACEID".into(), "COLCOUNT".into(),
        ];
        let rows = catalog.tables.iter().map(|t| vec![
            Value::Str(t.name.clone()),
            Value::Str(t.schemaname.clone()),
            Value::SmallInt(t.tbspaceid),
            Value::SmallInt(t.colcount),
        ]).collect();
        let column_index = Self::build_column_index(&column_names);
        CachedTable { column_names, column_index, rows }
    }

    fn materialize_syscolumns(catalog: &Catalog) -> CachedTable {
        let column_names: Vec<String> = vec![
            "NAME".into(), "TABNAME".into(), "SCHEMANAME".into(),
            "ORDINAL".into(), "TYPENAME".into(), "NULLABLE".into(),
        ];
        let rows = catalog.columns.iter().map(|c| vec![
            Value::Str(c.name.clone()),
            Value::Str(c.tabname.clone()),
            Value::Str(c.schemaname.clone()),
            Value::SmallInt(c.ordinal),
            Value::Str(c.typename.clone()),
            Value::Bool(c.nullable),
        ]).collect();
        let column_index = Self::build_column_index(&column_names);
        CachedTable { column_names, column_index, rows }
    }

    fn materialize_sysbufferpools(catalog: &Catalog) -> CachedTable {
        let column_names: Vec<String> = vec![
            "BPID".into(), "BPNAME".into(),
            "PAGESIZE".into(), "NPAGES".into(),
        ];
        let rows = catalog.bufferpools.iter().map(|bp| vec![
            Value::Integer(bp.bpid),
            Value::Str(bp.bpname.clone()),
            Value::Integer(bp.pagesize),
            Value::Integer(bp.npages),
        ]).collect();
        let column_index = Self::build_column_index(&column_names);
        CachedTable { column_names, column_index, rows }
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
                    name: "SYSTABLESPACES".into(),
                    schemaname: "RQSYS".into(),
                    tbspaceid: 1,
                    colcount: 7,
                },
                Table {
                    name: "SYSTABLES".into(),
                    schemaname: "RQSYS".into(),
                    tbspaceid: 1,
                    colcount: 4,
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
                    name: "NAME".into(),
                    tabname: "SYSTABLES".into(),
                    schemaname: "RQSYS".into(),
                    ordinal: 0,
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
    fn lookup_table_by_name() {
        let cache = CatalogCache::new(test_catalog());
        let t = cache.get_table("RQSYS", "SYSTABLESPACES").unwrap();
        assert_eq!(t.name, "SYSTABLESPACES");
        assert_eq!(t.colcount, 7);
        assert!(cache.get_table("RQSYS", "NONEXISTENT").is_none());
    }

    #[test]
    fn lookup_tablespace_by_id() {
        let cache = CatalogCache::new(test_catalog());
        let ts = cache.get_tablespace_by_id(1).unwrap();
        assert_eq!(ts.tbspace, "SYSTBSP");
        let ts2 = cache.get_tablespace_by_id(2).unwrap();
        assert_eq!(ts2.tbspace, "USERTBSP");
        assert!(cache.get_tablespace_by_id(99).is_none());
    }

    #[test]
    fn lookup_columns_sorted() {
        let cache = CatalogCache::new(test_catalog());
        let cols = cache.get_columns("RQSYS", "SYSTABLESPACES").unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].name, "TBSPACEID");
        assert_eq!(cols[1].name, "TBSPACE");
        // Ordinals are in order.
        assert!(cols[0].ordinal < cols[1].ordinal);
    }

    #[test]
    fn cached_table_data_matches() {
        let cache = CatalogCache::new(test_catalog());
        let ct = cache.get_table_data("RQSYS", "SYSTABLESPACES").unwrap();
        assert_eq!(ct.column_names.len(), 7);
        assert_eq!(ct.rows.len(), 2);
        // O(1) column index works.
        assert_eq!(*ct.column_index.get("TBSPACEID").unwrap(), 0);
        assert_eq!(*ct.column_index.get("BUFFERPOOLID").unwrap(), 6);
    }

    #[test]
    fn cached_table_not_found() {
        let cache = CatalogCache::new(test_catalog());
        assert!(cache.get_table_data("RQSYS", "NONEXISTENT").is_none());
    }

    #[test]
    fn schema_lookup() {
        let cache = CatalogCache::new(test_catalog());
        assert!(cache.schema_idx.contains_key("RQSYS"));
        assert!(!cache.schema_idx.contains_key("BOGUS"));
    }
}
