use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::storage::fsm::FreeSpaceMap;
use crate::storage::page::{
    PageId, PageRead, PageWrite, SlotIndex, SlottedPage,
    free_space_of, page_id_of,
};

/// Physical row address: (page number, slot index within that page).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rid {
    pub page_id: PageId,
    pub slot: SlotIndex,
}

/// Derive the FSM path from a `.DAT` path.
///
/// PostgreSQL keeps one FSM per relation, not per segment.  Our DAT files
/// follow the pattern `SCHEMA.TABLE.<fileid>.DAT`; the FSM drops the
/// file-ID segment to produce `SCHEMA.TABLE.FSM`.
///
/// For test paths without the multi-dot naming (e.g. `foo.DAT`), falls
/// back to a simple extension replacement.
pub fn fsm_path_for(dat_path: &Path) -> PathBuf {
    // Try to strip ".<fileid>.DAT" → "SCHEMA.TABLE.FSM"
    if let Some(stem) = dat_path.file_name().and_then(|n| n.to_str()) {
        // Pattern: "SCHEMA.TABLE.0.DAT" → parts = ["SCHEMA", "TABLE", "0", "DAT"]
        let parts: Vec<&str> = stem.split('.').collect();
        if parts.len() >= 4 && parts.last() == Some(&"DAT") {
            // Drop the last two segments (fileid + "DAT"), append "FSM"
            let base = parts[..parts.len() - 2].join(".");
            return dat_path.with_file_name(format!("{base}.FSM"));
        }
    }
    // Fallback: simple extension swap.
    dat_path.with_extension("FSM")
}

/// A heap file manages a single `.DAT` file as a sequence of slotted pages.
///
/// Each table maps to one heap file. Rows are addressed by [`Rid`].
pub struct HeapFile {
    file: File,
    page_size: usize,
    /// Total number of pages currently in the file.
    page_count: u64,
    /// Binary max-heap free-space map. O(log P) search and update.
    fsm: FreeSpaceMap,
    /// Path to the `.FSM` file (derived from the `.DAT` path).
    fsm_path: PathBuf,
}

impl HeapFile {
    /// Open an existing heap file or create a new one.
    ///
    /// Loads the FSM from the corresponding `.FSM` file if it exists,
    /// otherwise creates a new one with optimistic categories.
    pub fn open(path: &Path, page_size: usize) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        let file_len = file.metadata()?.len();
        let page_count = if page_size > 0 {
            file_len / page_size as u64
        } else {
            0
        };

        let fsm_path = fsm_path_for(path);
        let fsm = match FreeSpaceMap::load(&fsm_path)? {
            Some(mut loaded) => {
                // File may have grown since last save.
                if (page_count as usize) > loaded.page_count() {
                    loaded.extend(page_count as usize);
                }
                loaded
            }
            None => FreeSpaceMap::new(page_count as usize, page_size),
        };

        Ok(Self {
            file,
            page_size,
            page_count,
            fsm,
            fsm_path,
        })
    }

    pub fn page_size(&self) -> usize {
        self.page_size
    }

    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    /// Read a page from disk.
    pub fn read_page(&mut self, page_id: PageId) -> Result<SlottedPage> {
        if page_id >= self.page_count {
            return Err(Error::Corruption(format!(
                "page {page_id} out of range (file has {} pages)",
                self.page_count
            )));
        }
        let offset = page_id * self.page_size as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; self.page_size];
        self.file.read_exact(&mut buf)?;
        SlottedPage::from_bytes(buf)
    }

    /// Write a page to disk at its page_id position.
    pub fn write_page(&mut self, page: &SlottedPage) -> Result<()> {
        let page_id = page.page_id();
        let offset = page_id * self.page_size as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(page.as_bytes())?;
        self.file.flush()?;

        // Update page_count if this extends the file.
        if page_id >= self.page_count {
            self.page_count = page_id + 1;
            self.fsm.extend(self.page_count as usize);
        }
        // Record actual free space in the FSM.
        self.fsm.update(page_id as usize, page.free_space());
        Ok(())
    }

    /// Read a page directly into a caller-provided buffer (zero-allocation).
    ///
    /// Used by the buffer pool to read directly into pre-allocated frame memory.
    /// The caller is responsible for checksum verification.
    pub fn read_page_into(&mut self, page_id: PageId, buf: &mut [u8]) -> Result<()> {
        if page_id >= self.page_count {
            return Err(Error::Corruption(format!(
                "page {page_id} out of range (file has {} pages)",
                self.page_count
            )));
        }
        let offset = page_id * self.page_size as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(buf)?;
        Ok(())
    }

    /// Write raw page bytes to disk. The page_id is read from the buffer header.
    ///
    /// Used by the buffer pool to flush pre-allocated frame memory directly.
    pub fn write_page_buf(&mut self, buf: &[u8]) -> Result<()> {
        let page_id = page_id_of(buf);
        let offset = page_id * self.page_size as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(buf)?;
        self.file.flush()?;

        if page_id >= self.page_count {
            self.page_count = page_id + 1;
            self.fsm.extend(self.page_count as usize);
        }
        self.fsm.update(page_id as usize, free_space_of(buf));
        Ok(())
    }

    /// Insert a row into the heap, returning its RID.
    ///
    /// Uses the FSM binary max-heap to find a page with enough free
    /// space in **O(log P)**. Falls back to allocating a new page if
    /// no existing page qualifies.
    pub fn insert_row(&mut self, row: &[u8]) -> Result<Rid> {
        let needed = row.len();

        // O(log P) search for a candidate page.
        while let Some(pid) = self.fsm.search(needed) {
            let page_id = pid as u64;
            let mut page = self.read_page(page_id)?;
            if let Some(slot) = page.insert_row(row) {
                self.write_page(&page)?;
                return Ok(Rid {
                    page_id,
                    slot,
                });
            }
            // Optimistic category was wrong — correct it and retry.
            self.fsm.update(pid, page.free_space());
        }

        // No existing page has space — append a new one.
        let new_pid = self.page_count;
        let mut page = SlottedPage::new(new_pid, self.page_size);
        let slot = page
            .insert_row(row)
            .ok_or_else(|| Error::Catalog("row too large for a single page".into()))?;
        self.write_page(&page)?;
        Ok(Rid {
            page_id: new_pid,
            slot,
        })
    }

    /// Read a row by its RID.
    pub fn read_row(&mut self, rid: Rid) -> Result<Vec<u8>> {
        let page = self.read_page(rid.page_id)?;
        page.read_row(rid.slot)
            .map(|b| b.to_vec())
            .ok_or_else(|| {
                Error::Corruption(format!(
                    "row not found at page={}, slot={}",
                    rid.page_id, rid.slot
                ))
            })
    }

    /// Delete a row by its RID.
    pub fn delete_row(&mut self, rid: Rid) -> Result<bool> {
        let mut page = self.read_page(rid.page_id)?;
        let deleted = page.delete_row(rid.slot);
        if deleted {
            self.write_page(&page)?;
        }
        Ok(deleted)
    }

    /// Return an iterator over all live rows in the heap.
    pub fn scan(&mut self) -> Result<Vec<(Rid, Vec<u8>)>> {
        let mut rows = Vec::new();
        for pid in 0..self.page_count {
            let page = self.read_page(pid)?;
            for slot in 0..page.slot_count() {
                if let Some(data) = page.read_row(slot) {
                    rows.push((
                        Rid {
                            page_id: pid,
                            slot,
                        },
                        data.to_vec(),
                    ));
                }
            }
        }
        Ok(rows)
    }

    /// Persist the FSM to its `.FSM` file.
    pub fn save_fsm(&self) -> Result<()> {
        self.fsm.save(&self.fsm_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const TEST_PAGE_SIZE: usize = 256;

    /// Create a temp file path for a test and ensure it's cleaned up.
    struct TempFile {
        path: std::path::PathBuf,
    }

    impl TempFile {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("rustdb_test_{name}"));
            // Remove if leftover from a previous run.
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(fsm_path_for(&path));
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_file(fsm_path_for(&self.path));
        }
    }

    #[test]
    fn create_empty_heap() {
        let tmp = TempFile::new("create_empty");
        let hf = HeapFile::open(&tmp.path, TEST_PAGE_SIZE).unwrap();
        assert_eq!(hf.page_count(), 0);
    }

    #[test]
    fn insert_and_read_row() {
        let tmp = TempFile::new("insert_read");
        let mut hf = HeapFile::open(&tmp.path, TEST_PAGE_SIZE).unwrap();

        let rid = hf.insert_row(b"hello heap").unwrap();
        assert_eq!(rid.page_id, 0);
        assert_eq!(rid.slot, 0);
        assert_eq!(hf.page_count(), 1);

        let data = hf.read_row(rid).unwrap();
        assert_eq!(data, b"hello heap");
    }

    #[test]
    fn insert_multiple_rows_same_page() {
        let tmp = TempFile::new("multi_same");
        let mut hf = HeapFile::open(&tmp.path, TEST_PAGE_SIZE).unwrap();

        let r0 = hf.insert_row(b"row-0").unwrap();
        let r1 = hf.insert_row(b"row-1").unwrap();
        let r2 = hf.insert_row(b"row-2").unwrap();

        // All on page 0 (256 bytes is enough for 3 small rows).
        assert_eq!(r0.page_id, 0);
        assert_eq!(r1.page_id, 0);
        assert_eq!(r2.page_id, 0);
        assert_eq!(r0.slot, 0);
        assert_eq!(r1.slot, 1);
        assert_eq!(r2.slot, 2);

        assert_eq!(hf.read_row(r1).unwrap(), b"row-1");
    }

    #[test]
    fn rows_spill_to_new_page() {
        let tmp = TempFile::new("spill");
        let mut hf = HeapFile::open(&tmp.path, TEST_PAGE_SIZE).unwrap();

        // Fill rows until we spill past page 0.
        let row = [0xABu8; 40];
        let mut rids = Vec::new();
        for _ in 0..20 {
            rids.push(hf.insert_row(&row).unwrap());
        }

        assert!(hf.page_count() > 1, "should have spilled to a second page");
        // Every row is still readable.
        for rid in &rids {
            assert_eq!(hf.read_row(*rid).unwrap(), row);
        }
    }

    #[test]
    fn delete_and_scan() {
        let tmp = TempFile::new("delete_scan");
        let mut hf = HeapFile::open(&tmp.path, TEST_PAGE_SIZE).unwrap();

        let _r0 = hf.insert_row(b"keep").unwrap();
        let r1 = hf.insert_row(b"delete-me").unwrap();
        let _r2 = hf.insert_row(b"also-keep").unwrap();

        assert!(hf.delete_row(r1).unwrap());

        let rows = hf.scan().unwrap();
        let data: Vec<&[u8]> = rows.iter().map(|(_, d)| d.as_slice()).collect();
        assert_eq!(data, vec![b"keep".as_slice(), b"also-keep".as_slice()]);

        // The deleted row is gone.
        assert!(hf.read_row(r1).is_err());
    }

    #[test]
    fn reopen_heap_persists_data() {
        let tmp = TempFile::new("reopen");
        let rid;
        {
            let mut hf = HeapFile::open(&tmp.path, TEST_PAGE_SIZE).unwrap();
            rid = hf.insert_row(b"persisted").unwrap();
        }
        // Reopen from disk.
        let mut hf = HeapFile::open(&tmp.path, TEST_PAGE_SIZE).unwrap();
        assert_eq!(hf.page_count(), 1);
        assert_eq!(hf.read_row(rid).unwrap(), b"persisted");
    }

    #[test]
    fn scan_empty_heap() {
        let tmp = TempFile::new("scan_empty");
        let mut hf = HeapFile::open(&tmp.path, TEST_PAGE_SIZE).unwrap();
        let rows = hf.scan().unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn row_too_large_for_page() {
        let tmp = TempFile::new("too_large");
        let mut hf = HeapFile::open(&tmp.path, TEST_PAGE_SIZE).unwrap();
        let big = vec![0xFFu8; TEST_PAGE_SIZE]; // larger than usable space
        let err = hf.insert_row(&big).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }
}
