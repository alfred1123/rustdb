use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{Error, Result};
use crate::storage::page::{
    PageId, PageRead, PageWrite, SlotIndex, SlottedPage,
    PAGE_HEADER_SIZE, free_space_of, page_id_of,
};

/// Physical row address: (page number, slot index within that page).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rid {
    pub page_id: PageId,
    pub slot: SlotIndex,
}

/// A heap file manages a single `.DAT` file as a sequence of slotted pages.
///
/// Each table maps to one heap file. Rows are addressed by [`Rid`].
pub struct HeapFile {
    file: File,
    page_size: usize,
    /// Total number of pages currently in the file.
    page_count: u64,
    /// Per-page free-space directory: tracks usable bytes available in each page.
    /// Inserts compare `free_space[pid] >= row.len()` to skip pages that
    /// definitely cannot hold the row — no disk read required.
    free_space: Vec<u16>,
    /// Hint: index of the first page likely to have free space.
    /// Avoids re-scanning from page 0 on every insert.
    next_free_hint: usize,
}

impl HeapFile {
    /// Open an existing heap file or create a new one.
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

        // Optimistic: assume existing pages have maximum usable space.
        // The first real insert will read the page and correct the value.
        let max_usable = if page_size > PAGE_HEADER_SIZE { page_size - PAGE_HEADER_SIZE } else { 0 };
        let free_space = vec![max_usable as u16; page_count as usize];

        Ok(Self {
            file,
            page_size,
            page_count,
            free_space,
            next_free_hint: 0,
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
            self.free_space.resize(self.page_count as usize, 0);
        }
        // Record actual free space.
        self.free_space[page_id as usize] = page.free_space() as u16;
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
            self.free_space.resize(self.page_count as usize, 0);
        }
        self.free_space[page_id as usize] = free_space_of(buf) as u16;
        Ok(())
    }

    /// Insert a row into the heap, returning its RID.
    ///
    /// Uses the free-space directory to skip pages that cannot hold the row
    /// (no disk read needed). Starts from `next_free_hint` to avoid
    /// re-scanning known-full pages.
    pub fn insert_row(&mut self, row: &[u8]) -> Result<Rid> {
        let needed = row.len();

        // Scan from the hint forward, then wrap around.
        let count = self.page_count as usize;
        for i in 0..count {
            let pid = ((self.next_free_hint + i) % count) as u64;
            // Skip pages that definitely cannot hold this row.
            if (self.free_space[pid as usize] as usize) < needed {
                continue;
            }
            let mut page = self.read_page(pid)?;
            if let Some(slot) = page.insert_row(row) {
                self.write_page(&page)?;
                // Advance hint to this page (it may still have room).
                self.next_free_hint = pid as usize;
                return Ok(Rid {
                    page_id: pid,
                    slot,
                });
            }
            // Page didn't have enough room after all — correct its free space.
            self.free_space[pid as usize] = page.free_space() as u16;
        }

        // No existing page has space — append a new one.
        let new_pid = self.page_count;
        let mut page = SlottedPage::new(new_pid, self.page_size);
        let slot = page
            .insert_row(row)
            .ok_or_else(|| Error::Catalog("row too large for a single page".into()))?;
        self.write_page(&page)?;
        self.next_free_hint = new_pid as usize;
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
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
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
