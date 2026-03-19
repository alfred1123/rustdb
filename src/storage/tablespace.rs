use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::catalog::cache::CatalogCache;
use crate::error::{Error, Result};
use crate::storage::fsm::FreeSpaceMap;
use crate::storage::heap::Rid;
use crate::storage::page::{PageRead, PageWrite};
use crate::storage::pool::{BufferPoolId, BufferPoolManager, FileId};

/// Routing info for one table's data file.
struct TableFileInfo {
    pool_id: BufferPoolId,
    file_id: FileId,
    /// Binary max-heap free-space map. O(log P) search and update.
    fsm: FreeSpaceMap,
    /// Path to the `.FSM` file for persistence.
    fsm_path: PathBuf,
}

/// Central coordinator mapping (schema, table) to heap files and routing all
/// I/O through the buffer pool.
///
/// The tablespace manager is the single entry point for reading and writing
/// row data. Components above it (SQL executor, catalog layer) never touch
/// disk directly.
pub struct TablespaceManager {
    pool_manager: BufferPoolManager,
    /// (SCHEMA, TABLE) → file routing info.
    table_files: HashMap<(String, String), TableFileInfo>,
}

impl TablespaceManager {
    /// Build from catalog cache. Creates buffer pools, maps tablespaces to
    /// directories, and registers heap files for every table.
    pub fn open(data_dir: &Path, cache: &CatalogCache) -> Result<Self> {
        let catalog = cache.catalog();

        // 1. Create buffer pools from SYSBUFFERPOOLS.
        let mut pool_manager = BufferPoolManager::new();
        for bp in &catalog.bufferpools {
            pool_manager.create_pool(
                bp.bpid,
                &bp.bpname,
                bp.npages as usize,
                bp.pagesize as usize,
            )?;
        }

        // 2. Map tbspaceid → directory.
        let mut ts_dirs: HashMap<i32, std::path::PathBuf> = HashMap::new();
        for ts in &catalog.tablespaces {
            ts_dirs.insert(ts.tbspaceid, data_dir.join(ts.tbspace.to_lowercase()));
        }

        // 3. Register each table's DAT file with its tablespace's buffer pool.
        let mut table_files = HashMap::new();
        for table in &catalog.tables {
            let ts = cache
                .get_tablespace_by_id(table.tbspaceid as i32)
                .ok_or_else(|| {
                    Error::Catalog(format!(
                        "tablespace {} not found for table {}.{}",
                        table.tbspaceid, table.schemaname, table.name
                    ))
                })?;

            let dir = ts_dirs.get(&ts.tbspaceid).ok_or_else(|| {
                Error::Catalog(format!("no directory for tablespace {}", ts.tbspace))
            })?;

            let dat_path = dir.join(format!("{}.{}.0.DAT", table.schemaname, table.name));
            let fsm_path = dat_path.with_extension("FSM");
            let page_size = ts.pagesize as usize;
            let pool_id = ts.bufferpoolid;

            let file_id = pool_manager.register_file(pool_id, &dat_path, page_size)?;

            // Load FSM from disk or create optimistic one.
            let file_pages = pool_manager.get(pool_id)?.file_page_count(file_id)? as usize;
            let fsm = match FreeSpaceMap::load(&fsm_path)? {
                Some(mut loaded) => {
                    if file_pages > loaded.page_count() {
                        loaded.extend(file_pages);
                    }
                    loaded
                }
                None => FreeSpaceMap::new(file_pages, page_size),
            };

            table_files.insert(
                (table.schemaname.clone(), table.name.clone()),
                TableFileInfo {
                    pool_id,
                    file_id,
                    fsm,
                    fsm_path,
                },
            );
        }

        log::info!(
            "tablespace manager opened: {} tables across {} pools",
            table_files.len(),
            pool_manager.pool_ids().len(),
        );

        Ok(Self {
            pool_manager,
            table_files,
        })
    }

    /// Scan all live rows in a table, returning (RID, raw bytes) pairs.
    ///
    /// Reads every page through the buffer pool and extracts rows from
    /// each page's slot directory.
    pub fn table_scan(
        &mut self,
        schema: &str,
        table: &str,
    ) -> Result<Vec<(Rid, Vec<u8>)>> {
        let info = self.resolve(schema, table)?;
        let pool_id = info.pool_id;
        let file_id = info.file_id;

        let pool = self.pool_manager.get_mut(pool_id)?;
        let page_count = pool.file_page_count(file_id)?;

        let mut rows = Vec::new();
        for pid in 0..page_count {
            let page_rows: Vec<(u16, Vec<u8>)>;
            {
                let page = pool.fetch_page(file_id, pid)?;
                page_rows = (0..page.slot_count())
                    .filter_map(|slot| page.read_row(slot).map(|d| (slot, d.to_vec())))
                    .collect();
            }
            pool.unpin(file_id, pid, false)?;

            for (slot, data) in page_rows {
                rows.push((Rid { page_id: pid, slot }, data));
            }
        }

        Ok(rows)
    }

    /// Insert a row into a table, returning its RID.
    ///
    /// Uses the FSM binary max-heap to find a page with enough free
    /// space in **O(log P)**. Falls back to allocating a new page if
    /// no existing page qualifies.
    pub fn insert_row(
        &mut self,
        schema: &str,
        table: &str,
        row: &[u8],
    ) -> Result<Rid> {
        let key = (schema.to_string(), table.to_string());
        let info = self.table_files.get(&key).ok_or_else(|| {
            Error::Catalog(format!(
                "table {schema}.{table} not registered in tablespace manager"
            ))
        })?;
        let pool_id = info.pool_id;
        let file_id = info.file_id;
        let needed = row.len();

        // O(log P) search loop — retry if optimistic category was wrong.
        loop {
            let pid = match self.table_files.get(&key).unwrap().fsm.search(needed) {
                Some(p) => p,
                None => break, // no candidate → allocate new page
            };
            let page_id = pid as u64;

            let pool = self.pool_manager.get_mut(pool_id)?;
            let result: Option<u16>;
            let actual_free: usize;
            {
                let mut page = pool.fetch_page_mut(file_id, page_id)?;
                result = page.insert_row(row);
                actual_free = page.free_space();
            }

            if let Some(slot) = result {
                let pool = self.pool_manager.get_mut(pool_id)?;
                pool.unpin(file_id, page_id, true)?;
                let info = self.table_files.get_mut(&key).unwrap();
                info.fsm.update(pid, actual_free);
                return Ok(Rid { page_id, slot });
            }
            // Insert failed — correct FSM and retry.
            let pool = self.pool_manager.get_mut(pool_id)?;
            pool.unpin(file_id, page_id, false)?;
            let info = self.table_files.get_mut(&key).unwrap();
            info.fsm.update(pid, actual_free);
        }

        // No existing page has space — allocate a new one.
        let pool = self.pool_manager.get_mut(pool_id)?;
        let new_pid: u64;
        let slot: u16;
        let actual_free: usize;
        {
            let (pid, mut page) = pool.new_page(file_id)?;
            new_pid = pid;
            slot = page
                .insert_row(row)
                .ok_or_else(|| Error::Catalog("row too large for a single page".into()))?;
            actual_free = page.free_space();
        }
        pool.unpin(file_id, new_pid, true)?;

        let info = self.table_files.get_mut(&key).unwrap();
        info.fsm.extend(new_pid as usize + 1);
        info.fsm.update(new_pid as usize, actual_free);

        Ok(Rid {
            page_id: new_pid,
            slot,
        })
    }

    /// Read a single row by RID.
    pub fn read_row(
        &mut self,
        schema: &str,
        table: &str,
        rid: Rid,
    ) -> Result<Vec<u8>> {
        let info = self.resolve(schema, table)?;
        let pool_id = info.pool_id;
        let file_id = info.file_id;

        let pool = self.pool_manager.get_mut(pool_id)?;
        let data: Vec<u8>;
        {
            let page = pool.fetch_page(file_id, rid.page_id)?;
            data = page
                .read_row(rid.slot)
                .map(|b| b.to_vec())
                .ok_or_else(|| {
                    Error::Corruption(format!(
                        "row not found at page={}, slot={}",
                        rid.page_id, rid.slot
                    ))
                })?;
        }
        pool.unpin(file_id, rid.page_id, false)?;
        Ok(data)
    }

    /// Delete a row by RID.
    pub fn delete_row(
        &mut self,
        schema: &str,
        table: &str,
        rid: Rid,
    ) -> Result<bool> {
        let key = (schema.to_string(), table.to_string());
        let info = self.table_files.get(&key).ok_or_else(|| {
            Error::Catalog(format!(
                "table {schema}.{table} not registered in tablespace manager"
            ))
        })?;
        let pool_id = info.pool_id;
        let file_id = info.file_id;

        let pool = self.pool_manager.get_mut(pool_id)?;
        let deleted: bool;
        let actual_free: usize;
        {
            let mut page = pool.fetch_page_mut(file_id, rid.page_id)?;
            deleted = page.delete_row(rid.slot);
            actual_free = page.free_space();
        }
        pool.unpin(file_id, rid.page_id, deleted)?;

        // Update FSM — page now has more room.
        let info = self.table_files.get_mut(&key).unwrap();
        info.fsm.update(rid.page_id as usize, actual_free);

        Ok(deleted)
    }

    /// Flush all dirty pages across all buffer pools and persist FSMs.
    pub fn flush_all(&mut self) -> Result<()> {
        self.pool_manager.flush_all()?;
        for info in self.table_files.values() {
            info.fsm.save(&info.fsm_path)?;
        }
        Ok(())
    }

    /// Shared reference to the pool manager.
    pub fn pool_manager(&self) -> &BufferPoolManager {
        &self.pool_manager
    }

    fn resolve(&self, schema: &str, table: &str) -> Result<&TableFileInfo> {
        self.table_files
            .get(&(schema.to_string(), table.to_string()))
            .ok_or_else(|| {
                Error::Catalog(format!(
                    "table {schema}.{table} not registered in tablespace manager"
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    const PAGE_SIZE: usize = 256;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("rustdb_tsm_{name}"));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Helper: create a tablespace manager with one pool and one table file.
    fn setup(name: &str) -> (TempDir, TablespaceManager) {
        let dir = TempDir::new(name);
        let dat_path = dir.path.join("TEST.TESTTABLE.0.DAT");

        let mut pool_manager = BufferPoolManager::new();
        pool_manager
            .create_pool(1, "TESTBP", 16, PAGE_SIZE)
            .unwrap();
        let file_id = pool_manager
            .register_file(1, &dat_path, PAGE_SIZE)
            .unwrap();

        let mut table_files = HashMap::new();
        table_files.insert(
            ("TEST".to_string(), "TESTTABLE".to_string()),
            TableFileInfo {
                pool_id: 1,
                file_id,
                fsm: FreeSpaceMap::new(0, PAGE_SIZE),
                fsm_path: dat_path.with_extension("FSM"),
            },
        );

        let tsm = TablespaceManager {
            pool_manager,
            table_files,
        };
        (dir, tsm)
    }

    #[test]
    fn scan_empty_table() {
        let (_dir, mut tsm) = setup("scan_empty");
        let rows = tsm.table_scan("TEST", "TESTTABLE").unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn insert_and_read() {
        let (_dir, mut tsm) = setup("insert_read");
        let rid = tsm
            .insert_row("TEST", "TESTTABLE", b"hello world")
            .unwrap();
        assert_eq!(rid.page_id, 0);
        assert_eq!(rid.slot, 0);

        let data = tsm.read_row("TEST", "TESTTABLE", rid).unwrap();
        assert_eq!(data, b"hello world");
    }

    #[test]
    fn insert_and_scan() {
        let (_dir, mut tsm) = setup("insert_scan");
        let r0 = tsm.insert_row("TEST", "TESTTABLE", b"row-0").unwrap();
        let r1 = tsm.insert_row("TEST", "TESTTABLE", b"row-1").unwrap();
        let r2 = tsm.insert_row("TEST", "TESTTABLE", b"row-2").unwrap();

        let rows = tsm.table_scan("TEST", "TESTTABLE").unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], (r0, b"row-0".to_vec()));
        assert_eq!(rows[1], (r1, b"row-1".to_vec()));
        assert_eq!(rows[2], (r2, b"row-2".to_vec()));
    }

    #[test]
    fn delete_row_removes_from_scan() {
        let (_dir, mut tsm) = setup("delete");
        let _r0 = tsm.insert_row("TEST", "TESTTABLE", b"keep").unwrap();
        let r1 = tsm
            .insert_row("TEST", "TESTTABLE", b"drop-me")
            .unwrap();
        let _r2 = tsm
            .insert_row("TEST", "TESTTABLE", b"also-keep")
            .unwrap();

        assert!(tsm.delete_row("TEST", "TESTTABLE", r1).unwrap());

        let rows = tsm.table_scan("TEST", "TESTTABLE").unwrap();
        let data: Vec<&[u8]> = rows.iter().map(|(_, d)| d.as_slice()).collect();
        assert_eq!(data, vec![b"keep".as_slice(), b"also-keep".as_slice()]);
    }

    #[test]
    fn rows_spill_to_new_page() {
        let (_dir, mut tsm) = setup("spill");
        let row = [0xABu8; 40];
        let mut rids = Vec::new();
        for _ in 0..20 {
            rids.push(tsm.insert_row("TEST", "TESTTABLE", &row).unwrap());
        }

        let max_page = rids.iter().map(|r| r.page_id).max().unwrap();
        assert!(max_page > 0, "should have spilled to multiple pages");

        for rid in &rids {
            assert_eq!(tsm.read_row("TEST", "TESTTABLE", *rid).unwrap(), row);
        }
    }

    #[test]
    fn flush_persists_data() {
        let dir = TempDir::new("flush_persist");
        let dat_path = dir.path.join("TEST.PERSIST.0.DAT");

        let rid;
        {
            let mut pool_manager = BufferPoolManager::new();
            pool_manager
                .create_pool(1, "TESTBP", 16, PAGE_SIZE)
                .unwrap();
            let file_id = pool_manager
                .register_file(1, &dat_path, PAGE_SIZE)
                .unwrap();
            let mut table_files = HashMap::new();
            table_files.insert(
                ("TEST".to_string(), "PERSIST".to_string()),
                TableFileInfo {
                    pool_id: 1,
                    file_id,
                    fsm: FreeSpaceMap::new(0, PAGE_SIZE),
                    fsm_path: dat_path.with_extension("FSM"),
                },
            );
            let mut tsm = TablespaceManager {
                pool_manager,
                table_files,
            };
            rid = tsm
                .insert_row("TEST", "PERSIST", b"survive-restart")
                .unwrap();
            tsm.flush_all().unwrap();
        }

        // Reopen with a fresh manager pointing to the same file.
        {
            let mut pool_manager = BufferPoolManager::new();
            pool_manager
                .create_pool(1, "TESTBP", 16, PAGE_SIZE)
                .unwrap();
            let file_id = pool_manager
                .register_file(1, &dat_path, PAGE_SIZE)
                .unwrap();
            let mut table_files = HashMap::new();
            table_files.insert(
                ("TEST".to_string(), "PERSIST".to_string()),
                TableFileInfo {
                    pool_id: 1,
                    file_id,
                    fsm: FreeSpaceMap::load(&dat_path.with_extension("FSM")).unwrap()
                        .unwrap_or_else(|| FreeSpaceMap::new(0, PAGE_SIZE)),
                    fsm_path: dat_path.with_extension("FSM"),
                },
            );
            let mut tsm = TablespaceManager {
                pool_manager,
                table_files,
            };
            let data = tsm.read_row("TEST", "PERSIST", rid).unwrap();
            assert_eq!(data, b"survive-restart");
        }
    }

    #[test]
    fn table_not_found() {
        let (_dir, mut tsm) = setup("not_found");
        let err = tsm.table_scan("BAD", "NOTABLE").unwrap_err();
        assert!(err.to_string().contains("not registered"));
    }

    #[test]
    fn open_from_catalog() {
        let dir = TempDir::new("open_catalog");
        let cfg = crate::catalog::config::DbConfig::default();
        crate::catalog::bootstrap::bootstrap(&dir.path, &cfg).unwrap();
        let catalog =
            crate::catalog::loader::load_catalog(&dir.path, false, cfg.page_size).unwrap();
        let cache = crate::catalog::cache::CatalogCache::new(catalog);

        let tsm = TablespaceManager::open(&dir.path, &cache).unwrap();

        // All 5 RQSYS catalog tables are registered (page-based since Phase 5).
        assert_eq!(tsm.table_files.len(), 5);

        // All 4 buffer pools should be created.
        let pool_ids = tsm.pool_manager.pool_ids();
        assert_eq!(pool_ids.len(), 4);
    }

    #[test]
    fn row_too_large_for_page() {
        let (_dir, mut tsm) = setup("too_large");
        let big = vec![0xFFu8; PAGE_SIZE]; // exceeds usable space
        let err = tsm
            .insert_row("TEST", "TESTTABLE", &big)
            .unwrap_err();
        assert!(err.to_string().contains("too large"));
    }
}
