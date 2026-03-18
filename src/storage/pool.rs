use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::storage::heap::HeapFile;
use crate::storage::page::{PageId, SlottedPage};

/// Identifies a registered heap file within the buffer pool.
pub type FileId = u32;

/// Index into the frame array.
type FrameIndex = usize;

/// A single buffer frame holding one page.
struct Frame {
    /// The page data, or `None` if this frame is unused.
    page: Option<SlottedPage>,
    /// Which file this page belongs to.
    file_id: FileId,
    /// Which page within the file.
    page_id: PageId,
    /// Number of active pins — a pinned frame cannot be evicted.
    pin_count: u32,
    /// Whether the page has been modified since the last flush to disk.
    dirty: bool,
}

impl Frame {
    fn empty() -> Self {
        Self {
            page: None,
            file_id: 0,
            page_id: 0,
            pin_count: 0,
            dirty: false,
        }
    }

    fn is_free(&self) -> bool {
        self.page.is_none()
    }
}

/// A registered file in the pool.
struct FileEntry {
    path: PathBuf,
    page_size: usize,
    heap: HeapFile,
}

/// A fixed-size buffer pool that mediates all page I/O.
///
/// Every component above the buffer pool accesses pages through `fetch_page`
/// and releases them via `unpin`. Dirty pages are flushed asynchronously on
/// eviction or explicitly via `flush_page` / `flush_all`.
pub struct BufferPool {
    /// Fixed-size array of page frames.
    frames: Vec<Frame>,
    /// Maps (file_id, page_id) → frame index for fast lookup.
    page_table: HashMap<(FileId, PageId), FrameIndex>,
    /// LRU list of *unpinned* frame indices — front is the oldest (eviction victim).
    lru: VecDeque<FrameIndex>,
    /// Registered heap files.
    files: HashMap<FileId, FileEntry>,
    /// Next file ID to assign.
    next_file_id: FileId,
}

impl BufferPool {
    /// Create a new buffer pool with `capacity` page frames.
    pub fn new(capacity: usize) -> Self {
        let frames = (0..capacity).map(|_| Frame::empty()).collect::<Vec<_>>();
        let lru = (0..capacity).collect::<VecDeque<_>>();
        Self {
            frames,
            page_table: HashMap::new(),
            lru,
            files: HashMap::new(),
            next_file_id: 0,
        }
    }

    /// Register a heap file with the pool. Returns a `FileId` handle.
    pub fn register_file(&mut self, path: &Path, page_size: usize) -> Result<FileId> {
        let heap = HeapFile::open(path, page_size)?;
        let fid = self.next_file_id;
        self.next_file_id += 1;
        self.files.insert(
            fid,
            FileEntry {
                path: path.to_path_buf(),
                page_size,
                heap,
            },
        );
        Ok(fid)
    }

    /// Fetch a page into the pool and pin it. Returns a shared reference.
    ///
    /// If the page is already in a frame, the existing frame is re-pinned.
    /// Otherwise an LRU victim is evicted (dirty pages are flushed first)
    /// and the requested page is loaded from disk.
    pub fn fetch_page(&mut self, file_id: FileId, page_id: PageId) -> Result<&SlottedPage> {
        let frame_idx = self.ensure_loaded(file_id, page_id)?;
        // SAFETY of the index: ensure_loaded just validated it.
        let frame = &mut self.frames[frame_idx];
        frame.pin_count += 1;
        // Remove from LRU if present (it's now pinned).
        self.lru.retain(|&i| i != frame_idx);
        Ok(self.frames[frame_idx].page.as_ref().unwrap())
    }

    /// Fetch a page for mutation. The page is automatically marked dirty.
    pub fn fetch_page_mut(
        &mut self,
        file_id: FileId,
        page_id: PageId,
    ) -> Result<&mut SlottedPage> {
        let frame_idx = self.ensure_loaded(file_id, page_id)?;
        let frame = &mut self.frames[frame_idx];
        frame.pin_count += 1;
        frame.dirty = true;
        self.lru.retain(|&i| i != frame_idx);
        Ok(self.frames[frame_idx].page.as_mut().unwrap())
    }

    /// Allocate a brand-new page in the given file. The page is loaded into
    /// the pool already pinned and marked dirty.
    pub fn new_page(&mut self, file_id: FileId) -> Result<(PageId, &mut SlottedPage)> {
        let entry = self.files.get(&file_id).ok_or_else(|| {
            Error::Catalog(format!("file_id {file_id} not registered"))
        })?;
        let new_pid = entry.heap.page_count();
        let page_size = entry.page_size;

        let frame_idx = self.evict_for_frame()?;
        let frame = &mut self.frames[frame_idx];

        let page = SlottedPage::new(new_pid, page_size);
        frame.page = Some(page);
        frame.file_id = file_id;
        frame.page_id = new_pid;
        frame.pin_count = 1;
        frame.dirty = true;

        self.page_table.insert((file_id, new_pid), frame_idx);

        Ok((new_pid, self.frames[frame_idx].page.as_mut().unwrap()))
    }

    /// Decrement the pin count for a page. When `dirty` is true the frame is
    /// marked dirty so it will be flushed before eviction.
    ///
    /// Once pin_count reaches 0 the frame enters the LRU list and becomes
    /// eligible for eviction.
    pub fn unpin(&mut self, file_id: FileId, page_id: PageId, dirty: bool) -> Result<()> {
        let frame_idx = *self
            .page_table
            .get(&(file_id, page_id))
            .ok_or_else(|| Error::Catalog(format!("page ({file_id}, {page_id}) not in pool")))?;
        let frame = &mut self.frames[frame_idx];
        if frame.pin_count == 0 {
            return Err(Error::Catalog("unpin called on unpinned frame".into()));
        }
        if dirty {
            frame.dirty = true;
        }
        frame.pin_count -= 1;
        if frame.pin_count == 0 {
            // Add to the back of the LRU list (most-recently used).
            self.lru.push_back(frame_idx);
        }
        Ok(())
    }

    /// Flush a single page to disk (if dirty).
    pub fn flush_page(&mut self, file_id: FileId, page_id: PageId) -> Result<()> {
        let frame_idx = *self
            .page_table
            .get(&(file_id, page_id))
            .ok_or_else(|| Error::Catalog(format!("page ({file_id}, {page_id}) not in pool")))?;
        self.flush_frame(frame_idx)
    }

    /// Flush **all** dirty pages to disk.
    pub fn flush_all(&mut self) -> Result<()> {
        let indices: Vec<FrameIndex> = (0..self.frames.len())
            .filter(|&i| self.frames[i].dirty)
            .collect();
        for idx in indices {
            self.flush_frame(idx)?;
        }
        Ok(())
    }

    /// Returns the number of frames currently in use (not free).
    pub fn used_frames(&self) -> usize {
        self.frames.iter().filter(|f| !f.is_free()).count()
    }

    /// Returns the pool capacity (total number of frames).
    pub fn capacity(&self) -> usize {
        self.frames.len()
    }

    // ── Internal helpers ──

    /// Ensure the requested page is present in a frame. Returns the frame index.
    fn ensure_loaded(&mut self, file_id: FileId, page_id: PageId) -> Result<FrameIndex> {
        // Fast path: already in the pool.
        if let Some(&idx) = self.page_table.get(&(file_id, page_id)) {
            return Ok(idx);
        }

        // Slow path: need to load from disk → find a frame.
        let frame_idx = self.evict_for_frame()?;

        let entry = self.files.get_mut(&file_id).ok_or_else(|| {
            Error::Catalog(format!("file_id {file_id} not registered"))
        })?;
        let page = entry.heap.read_page(page_id)?;

        let frame = &mut self.frames[frame_idx];
        frame.page = Some(page);
        frame.file_id = file_id;
        frame.page_id = page_id;
        frame.pin_count = 0;
        frame.dirty = false;

        self.page_table.insert((file_id, page_id), frame_idx);
        Ok(frame_idx)
    }

    /// Find a usable frame, evicting an LRU victim if necessary.
    fn evict_for_frame(&mut self) -> Result<FrameIndex> {
        // First check for a completely free frame.
        if let Some(idx) = self.frames.iter().position(|f| f.is_free()) {
            // Remove from LRU (free frames are in LRU at startup).
            self.lru.retain(|&i| i != idx);
            return Ok(idx);
        }

        // Evict the least-recently-used unpinned frame.
        let victim = self
            .lru
            .pop_front()
            .ok_or_else(|| Error::Catalog("buffer pool full: all frames are pinned".into()))?;

        // Flush dirty victim before reuse.
        if self.frames[victim].dirty {
            self.flush_frame(victim)?;
        }

        // Remove old mapping.
        let old_key = (self.frames[victim].file_id, self.frames[victim].page_id);
        self.page_table.remove(&old_key);

        // Reset frame.
        self.frames[victim].page = None;
        self.frames[victim].pin_count = 0;
        self.frames[victim].dirty = false;

        Ok(victim)
    }

    /// Write a frame's page to its heap file and clear the dirty flag.
    fn flush_frame(&mut self, idx: FrameIndex) -> Result<()> {
        let frame = &self.frames[idx];
        if !frame.dirty {
            return Ok(());
        }
        let page = frame.page.as_ref().ok_or_else(|| {
            Error::Corruption("flush on empty frame".into())
        })?;
        let fid = frame.file_id;

        let entry = self.files.get_mut(&fid).ok_or_else(|| {
            Error::Catalog(format!("file_id {fid} not registered (flush)"))
        })?;
        entry.heap.write_page(page)?;
        self.frames[idx].dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const PAGE_SIZE: usize = 256;

    /// Temp file with automatic cleanup.
    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("rustdb_pool_{name}"));
            let _ = fs::remove_file(&path);
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    /// Helper: register a file and create N pages on disk via HeapFile directly.
    fn setup_file(pool: &mut BufferPool, name: &str, n_pages: u64) -> (TempFile, FileId) {
        let tmp = TempFile::new(name);
        // Pre-populate pages on disk.
        {
            let mut hf = HeapFile::open(&tmp.path, PAGE_SIZE).unwrap();
            for i in 0..n_pages {
                let page = SlottedPage::new(i, PAGE_SIZE);
                hf.write_page(&page).unwrap();
            }
        }
        let fid = pool.register_file(&tmp.path, PAGE_SIZE).unwrap();
        (tmp, fid)
    }

    #[test]
    fn fetch_and_unpin() {
        let mut pool = BufferPool::new(4);
        let (_tmp, fid) = setup_file(&mut pool, "fetch_unpin", 2);

        let page = pool.fetch_page(fid, 0).unwrap();
        assert_eq!(page.page_id(), 0);

        pool.unpin(fid, 0, false).unwrap();
    }

    #[test]
    fn fetch_same_page_twice() {
        let mut pool = BufferPool::new(4);
        let (_tmp, fid) = setup_file(&mut pool, "fetch_twice", 1);

        let _p1 = pool.fetch_page(fid, 0).unwrap();
        pool.unpin(fid, 0, false).unwrap();

        // Second fetch should hit the pool (same frame).
        let _p2 = pool.fetch_page(fid, 0).unwrap();
        pool.unpin(fid, 0, false).unwrap();

        // Only 1 frame in use.
        assert_eq!(pool.used_frames(), 1);
    }

    #[test]
    fn dirty_flag_preserved_across_unpin() {
        let mut pool = BufferPool::new(4);
        let (_tmp, fid) = setup_file(&mut pool, "dirty_flag", 1);

        let _page = pool.fetch_page(fid, 0).unwrap();
        pool.unpin(fid, 0, true).unwrap();

        // Frame should still be dirty.
        let idx = pool.page_table[&(fid, 0)];
        assert!(pool.frames[idx].dirty);
    }

    #[test]
    fn flush_page_clears_dirty() {
        let mut pool = BufferPool::new(4);
        let (_tmp, fid) = setup_file(&mut pool, "flush_dirty", 1);

        let _page = pool.fetch_page(fid, 0).unwrap();
        pool.unpin(fid, 0, true).unwrap();
        pool.flush_page(fid, 0).unwrap();

        let idx = pool.page_table[&(fid, 0)];
        assert!(!pool.frames[idx].dirty);
    }

    #[test]
    fn eviction_flushes_dirty_page() {
        // Pool with only 1 frame — fetching a second page forces eviction.
        let mut pool = BufferPool::new(1);
        let (_tmp, fid) = setup_file(&mut pool, "evict_dirty", 2);

        // Fetch page 0, mark dirty, unpin.
        {
            let page = pool.fetch_page_mut(fid, 0).unwrap();
            page.insert_row(b"dirty-data");
        }
        pool.unpin(fid, 0, true).unwrap();

        // Fetch page 1 — page 0 must be evicted and flushed.
        let _p1 = pool.fetch_page(fid, 1).unwrap();
        pool.unpin(fid, 1, false).unwrap();

        // Page 0 was evicted. Reload from disk and verify the written data.
        let _p0 = pool.fetch_page(fid, 0).unwrap();
        pool.unpin(fid, 0, false).unwrap();

        // Read through HeapFile to confirm disk persistence.
        let entry = pool.files.get_mut(&fid).unwrap();
        let disk_page = entry.heap.read_page(0).unwrap();
        assert_eq!(disk_page.read_row(0).unwrap(), b"dirty-data");
    }

    #[test]
    fn lru_evicts_oldest_unpinned() {
        let mut pool = BufferPool::new(2);
        let (_tmp, fid) = setup_file(&mut pool, "lru_order", 3);

        // Load pages 0 and 1.
        let _p0 = pool.fetch_page(fid, 0).unwrap();
        pool.unpin(fid, 0, false).unwrap();
        let _p1 = pool.fetch_page(fid, 1).unwrap();
        pool.unpin(fid, 1, false).unwrap();

        // Access page 0 again to make it more recent.
        let _p0 = pool.fetch_page(fid, 0).unwrap();
        pool.unpin(fid, 0, false).unwrap();

        // Fetch page 2 — page 1 (oldest) should be evicted, not page 0.
        let _p2 = pool.fetch_page(fid, 2).unwrap();
        pool.unpin(fid, 2, false).unwrap();

        assert!(pool.page_table.contains_key(&(fid, 0)));
        assert!(!pool.page_table.contains_key(&(fid, 1)));
        assert!(pool.page_table.contains_key(&(fid, 2)));
    }

    #[test]
    fn all_pinned_returns_error() {
        let mut pool = BufferPool::new(1);
        let (_tmp, fid) = setup_file(&mut pool, "all_pinned", 2);

        let _p0 = pool.fetch_page(fid, 0).unwrap();
        // Don't unpin — pool is full and all pinned.

        let err = pool.fetch_page(fid, 1).unwrap_err();
        assert!(err.to_string().contains("all frames are pinned"));

        pool.unpin(fid, 0, false).unwrap();
    }

    #[test]
    fn new_page_creates_and_pins() {
        let mut pool = BufferPool::new(4);
        let (_tmp, fid) = setup_file(&mut pool, "new_page", 0);

        let (pid, page) = pool.new_page(fid).unwrap();
        assert_eq!(pid, 0);
        assert_eq!(page.page_id(), 0);

        // Frame is dirty and pinned.
        let idx = pool.page_table[&(fid, pid)];
        assert!(pool.frames[idx].dirty);
        assert_eq!(pool.frames[idx].pin_count, 1);

        pool.unpin(fid, pid, true).unwrap();
    }

    #[test]
    fn flush_all_writes_all_dirty() {
        let mut pool = BufferPool::new(4);
        let (_tmp, fid) = setup_file(&mut pool, "flush_all", 2);

        // Mark both pages dirty.
        {
            let p0 = pool.fetch_page_mut(fid, 0).unwrap();
            p0.insert_row(b"d0");
        }
        pool.unpin(fid, 0, true).unwrap();
        {
            let p1 = pool.fetch_page_mut(fid, 1).unwrap();
            p1.insert_row(b"d1");
        }
        pool.unpin(fid, 1, true).unwrap();

        pool.flush_all().unwrap();

        // Both frames should be clean now.
        for idx in 0..pool.frames.len() {
            if !pool.frames[idx].is_free() {
                assert!(!pool.frames[idx].dirty);
            }
        }
    }

    #[test]
    fn multiple_files() {
        let mut pool = BufferPool::new(8);
        let (_tmp1, fid1) = setup_file(&mut pool, "multi_f1", 2);
        let (_tmp2, fid2) = setup_file(&mut pool, "multi_f2", 2);

        let _p = pool.fetch_page(fid1, 0).unwrap();
        pool.unpin(fid1, 0, false).unwrap();

        let _p = pool.fetch_page(fid2, 1).unwrap();
        pool.unpin(fid2, 1, false).unwrap();

        assert!(pool.page_table.contains_key(&(fid1, 0)));
        assert!(pool.page_table.contains_key(&(fid2, 1)));
    }

    #[test]
    fn unpin_twice_errors() {
        let mut pool = BufferPool::new(4);
        let (_tmp, fid) = setup_file(&mut pool, "unpin_twice", 1);

        let _p = pool.fetch_page(fid, 0).unwrap();
        pool.unpin(fid, 0, false).unwrap();

        // Second unpin should fail (pin_count already 0).
        let err = pool.unpin(fid, 0, false).unwrap_err();
        assert!(err.to_string().contains("unpinned"));
    }

    #[test]
    fn fetch_mut_marks_dirty() {
        let mut pool = BufferPool::new(4);
        let (_tmp, fid) = setup_file(&mut pool, "fetch_mut", 1);

        let _page = pool.fetch_page_mut(fid, 0).unwrap();
        let idx = pool.page_table[&(fid, 0)];
        assert!(pool.frames[idx].dirty);
        pool.unpin(fid, 0, false).unwrap();
    }
}
