use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::storage::heap::HeapFile;
use crate::storage::page::{
    PageId, PageMut, PageRef,
    init_page_buf, verify_checksum_of,
};

/// Identifies a registered heap file within the buffer pool.
pub type FileId = u32;

/// Identifies a buffer pool within the `BufferPoolManager`.
pub type BufferPoolId = i32;

/// Index into the frame metadata array.
type FrameIndex = usize;

/// Access mode for a frame latch (readers–writer).
///
/// Strict ACID model: a frame is either shared-read or exclusive-write.
/// Uncommitted-read (read while exclusive-write held) is not yet supported
/// and can be added later as a separate isolation level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LatchMode {
    /// No active latch — frame is idle or only in LRU.
    None,
    /// One or more readers hold the frame (pin_count tracks how many).
    Shared,
    /// Exactly one writer holds the frame (pin_count must be 1).
    Exclusive,
}

/// Metadata for a single buffer frame. The actual page data lives in
/// `BufferPool::pool_buf` at offset `frame_index * page_size`.
struct FrameMeta {
    /// Which file this page belongs to.
    file_id: FileId,
    /// Which page within the file.
    page_id: PageId,
    /// Number of active pins — a pinned frame cannot be evicted.
    pin_count: u32,
    /// Whether the page has been modified since the last flush to disk.
    dirty: bool,
    /// Whether this frame currently holds a valid page.
    in_use: bool,
    /// Current latch mode — enforces readers–writer exclusion.
    latch: LatchMode,
}

impl FrameMeta {
    fn empty() -> Self {
        Self {
            file_id: 0,
            page_id: 0,
            pin_count: 0,
            dirty: false,
            in_use: false,
            latch: LatchMode::None,
        }
    }
}

/// A registered file in the pool.
struct FileEntry {
    #[allow(dead_code)]
    path: PathBuf,
    heap: HeapFile,
    /// Logical page count — includes pages allocated via `new_page`
    /// that may not have been flushed to disk yet.
    page_count: u64,
}

/// A fixed-size, pre-allocated buffer pool that mediates all page I/O.
///
/// All `capacity × page_size` bytes are allocated **once** at construction.
/// Each frame owns a fixed slice of this contiguous region — no per-page
/// heap allocation occurs on the fetch/evict hot path.
///
/// Every component above the buffer pool accesses pages through `fetch_page`
/// and releases them via `unpin`. Dirty pages are flushed lazily on eviction
/// or explicitly via `flush_page` / `flush_all`.
pub struct BufferPool {
    /// Human-readable name for this pool (e.g. "RQDEFAULTBP", "INDEXBP").
    name: String,
    /// Single contiguous allocation: `capacity * page_size` bytes.
    /// Frame `i` occupies `pool_buf[i*page_size .. (i+1)*page_size]`.
    pool_buf: Vec<u8>,
    /// Per-frame metadata (file_id, page_id, pin_count, dirty, in_use).
    frame_meta: Vec<FrameMeta>,
    /// Fixed page size for every frame in this pool.
    page_size: usize,
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
    /// Create a new buffer pool with `capacity` page frames, each `page_size`
    /// bytes. The entire memory region (`capacity * page_size`) is allocated
    /// upfront.
    pub fn new(name: &str, capacity: usize, page_size: usize) -> Self {
        let pool_buf = vec![0u8; capacity * page_size];
        let frame_meta = (0..capacity).map(|_| FrameMeta::empty()).collect::<Vec<_>>();
        let lru = (0..capacity).collect::<VecDeque<_>>();
        Self {
            name: name.to_string(),
            pool_buf,
            frame_meta,
            page_size,
            page_table: HashMap::new(),
            lru,
            files: HashMap::new(),
            next_file_id: 0,
        }
    }

    /// Register a heap file with the pool. Returns a `FileId` handle.
    ///
    /// The file's page size must match the pool's page size.
    pub fn register_file(&mut self, path: &Path, page_size: usize) -> Result<FileId> {
        if page_size != self.page_size {
            return Err(Error::Catalog(format!(
                "page size mismatch: pool uses {} but file requests {}",
                self.page_size, page_size
            )));
        }
        let heap = HeapFile::open(path, page_size)?;
        let page_count = heap.page_count();
        let fid = self.next_file_id;
        self.next_file_id += 1;
        self.files.insert(
            fid,
            FileEntry {
                path: path.to_path_buf(),
                heap,
                page_count,
            },
        );
        Ok(fid)
    }

    /// Fetch a page into the pool and pin it. Returns a read-only view.
    ///
    /// If the page is already in a frame, the existing frame is re-pinned.
    /// Otherwise an LRU victim is evicted (dirty pages are flushed first)
    /// and the requested page is loaded from disk into the pre-allocated frame.
    pub fn fetch_page(&mut self, file_id: FileId, page_id: PageId) -> Result<PageRef<'_>> {
        let frame_idx = self.ensure_loaded(file_id, page_id)?;
        // Enforce latch: shared read is allowed only when no exclusive writer.
        if self.frame_meta[frame_idx].latch == LatchMode::Exclusive {
            return Err(Error::Catalog(format!(
                "page ({file_id}, {page_id}) is exclusively latched for write"
            )));
        }
        self.frame_meta[frame_idx].pin_count += 1;
        self.frame_meta[frame_idx].latch = LatchMode::Shared;
        // Remove from LRU if present (it's now pinned).
        self.lru.retain(|&i| i != frame_idx);
        let off = frame_idx * self.page_size;
        Ok(PageRef::new(&self.pool_buf[off..off + self.page_size]))
    }

    /// Fetch a page for mutation. The page is automatically marked dirty.
    /// Returns a mutable view backed by the pre-allocated frame memory.
    pub fn fetch_page_mut(
        &mut self,
        file_id: FileId,
        page_id: PageId,
    ) -> Result<PageMut<'_>> {
        let frame_idx = self.ensure_loaded(file_id, page_id)?;
        // Enforce latch: exclusive write requires no other readers or writers.
        if self.frame_meta[frame_idx].pin_count > 0 {
            return Err(Error::Catalog(format!(
                "page ({file_id}, {page_id}) is already pinned — \
                 cannot acquire exclusive write latch"
            )));
        }
        self.frame_meta[frame_idx].pin_count = 1;
        self.frame_meta[frame_idx].latch = LatchMode::Exclusive;
        self.frame_meta[frame_idx].dirty = true;
        self.lru.retain(|&i| i != frame_idx);
        let off = frame_idx * self.page_size;
        Ok(PageMut::new(&mut self.pool_buf[off..off + self.page_size]))
    }

    /// Allocate a brand-new page in the given file. The page is initialized
    /// in the pre-allocated frame, already pinned and marked dirty.
    pub fn new_page(&mut self, file_id: FileId) -> Result<(PageId, PageMut<'_>)> {
        let new_pid = {
            let entry = self.files.get_mut(&file_id)
                .ok_or_else(|| Error::Catalog(format!("file_id {file_id} not registered")))?;
            let pid = entry.page_count;
            entry.page_count += 1;
            pid
        };
        let page_size = self.page_size;

        let frame_idx = self.evict_for_frame()?;
        let off = frame_idx * page_size;

        // Initialize the page directly in the pool buffer.
        init_page_buf(&mut self.pool_buf[off..off + page_size], new_pid);

        self.frame_meta[frame_idx].file_id = file_id;
        self.frame_meta[frame_idx].page_id = new_pid;
        self.frame_meta[frame_idx].pin_count = 1;
        self.frame_meta[frame_idx].dirty = true;
        self.frame_meta[frame_idx].in_use = true;
        self.frame_meta[frame_idx].latch = LatchMode::Exclusive;

        self.page_table.insert((file_id, new_pid), frame_idx);

        Ok((new_pid, PageMut::new(&mut self.pool_buf[off..off + page_size])))
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
        if self.frame_meta[frame_idx].pin_count == 0 {
            return Err(Error::Catalog("unpin called on unpinned frame".into()));
        }
        if dirty {
            self.frame_meta[frame_idx].dirty = true;
        }
        self.frame_meta[frame_idx].pin_count -= 1;
        if self.frame_meta[frame_idx].pin_count == 0 {
            self.frame_meta[frame_idx].latch = LatchMode::None;
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
        let indices: Vec<FrameIndex> = (0..self.frame_meta.len())
            .filter(|&i| self.frame_meta[i].dirty)
            .collect();
        for idx in indices {
            self.flush_frame(idx)?;
        }
        Ok(())
    }

    /// Returns the number of frames currently in use (holding a page).
    pub fn used_frames(&self) -> usize {
        self.frame_meta.iter().filter(|m| m.in_use).count()
    }

    /// Returns the pool capacity (total number of frames).
    pub fn capacity(&self) -> usize {
        self.frame_meta.len()
    }

    /// Returns the fixed page size for this pool.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Returns the pool name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the logical page count for a registered file.
    ///
    /// This includes pages allocated via `new_page` that may not have been
    /// flushed to disk yet.
    pub fn file_page_count(&self, file_id: FileId) -> Result<u64> {
        self.files
            .get(&file_id)
            .map(|e| e.page_count)
            .ok_or_else(|| Error::Catalog(format!("file_id {file_id} not registered")))
    }

    /// Evict all pages belonging to a file from the pool and remove the
    /// file registration. Dirty pages are **discarded** (not flushed) — the
    /// caller is expected to be deleting the file (e.g., DROP TABLE).
    pub fn evict_file(&mut self, file_id: FileId) -> Result<()> {
        let frame_indices: Vec<FrameIndex> = self.page_table.iter()
            .filter(|&(&(fid, _), _)| fid == file_id)
            .map(|(_, &idx)| idx)
            .collect();

        for idx in &frame_indices {
            let key = (self.frame_meta[*idx].file_id, self.frame_meta[*idx].page_id);
            self.page_table.remove(&key);
            self.lru.retain(|&i| i != *idx);
            self.frame_meta[*idx] = FrameMeta::empty();
        }

        self.files.remove(&file_id);
        Ok(())
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
        let page_size = self.page_size;
        let off = frame_idx * page_size;

        // Read page directly into the pre-allocated frame — zero extra allocation.
        {
            let entry = self.files.get_mut(&file_id).ok_or_else(|| {
                Error::Catalog(format!("file_id {file_id} not registered"))
            })?;
            let frame_buf = &mut self.pool_buf[off..off + page_size];
            entry.heap.read_page_into(page_id, frame_buf)?;
        }

        // Verify checksum on the in-place data.
        verify_checksum_of(&self.pool_buf[off..off + page_size])?;

        self.frame_meta[frame_idx].file_id = file_id;
        self.frame_meta[frame_idx].page_id = page_id;
        self.frame_meta[frame_idx].pin_count = 0;
        self.frame_meta[frame_idx].dirty = false;
        self.frame_meta[frame_idx].in_use = true;
        self.frame_meta[frame_idx].latch = LatchMode::None;

        self.page_table.insert((file_id, page_id), frame_idx);
        Ok(frame_idx)
    }

    /// Find a usable frame, evicting an LRU victim if necessary.
    fn evict_for_frame(&mut self) -> Result<FrameIndex> {
        // First check for a free frame.
        if let Some(idx) = self.frame_meta.iter().position(|m| !m.in_use) {
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
        if self.frame_meta[victim].dirty {
            self.flush_frame(victim)?;
        }

        // Remove old mapping.
        let old_key = (self.frame_meta[victim].file_id, self.frame_meta[victim].page_id);
        self.page_table.remove(&old_key);

        // Mark frame as free.
        self.frame_meta[victim].in_use = false;
        self.frame_meta[victim].pin_count = 0;
        self.frame_meta[victim].dirty = false;
        self.frame_meta[victim].latch = LatchMode::None;

        Ok(victim)
    }

    /// Write a frame's page data to its heap file and clear the dirty flag.
    fn flush_frame(&mut self, idx: FrameIndex) -> Result<()> {
        if !self.frame_meta[idx].dirty {
            return Ok(());
        }
        if !self.frame_meta[idx].in_use {
            return Err(Error::Corruption("flush on empty frame".into()));
        }
        let fid = self.frame_meta[idx].file_id;
        let page_size = self.page_size;
        let off = idx * page_size;

        {
            let entry = self.files.get_mut(&fid).ok_or_else(|| {
                Error::Catalog(format!("file_id {fid} not registered (flush)"))
            })?;
            entry.heap.write_page_buf(&self.pool_buf[off..off + page_size])?;
        }

        self.frame_meta[idx].dirty = false;
        Ok(())
    }
}

// ── BufferPoolManager ──
//
// Manages multiple named buffer pools, each with its own page size and
// capacity. In a transactional DB, different workloads need separate pools:
//
//   ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐
//   │ RQDEFAULTBP      │  │ INDEXBP           │  │ LOBBP            │
//   │ 4K pages, 128 fr │  │ 4K pages, 64 fr  │  │ 32K pages, 32 fr│
//   └────────┬─────────┘  └────────┬─────────┘  └────────┬─────────┘
//            │                     │                      │
//     data tablespaces      index tablespaces      LOB tablespaces
//
// The manager maps `BufferPoolId` → `BufferPool`, and each tablespace is
// associated with a pool via `SYSTABLESPACES.BUFFERPOOLID`.

/// Manages multiple pre-allocated buffer pools for different data workloads.
///
/// Each tablespace is assigned to a `BufferPoolId`, which maps to a
/// `BufferPool` with its own page size and memory allocation. This allows
/// separate tuning for data, index, LOB, and temporary workloads.
///
/// The default pool (id=1) is named `RQDEFAULTBP`.
pub struct BufferPoolManager {
    pools: HashMap<BufferPoolId, BufferPool>,
}

impl BufferPoolManager {
    /// Create a new manager with no pools. Use `create_pool` to add them.
    pub fn new() -> Self {
        Self {
            pools: HashMap::new(),
        }
    }

    /// Create and register a new buffer pool.
    pub fn create_pool(
        &mut self,
        id: BufferPoolId,
        name: &str,
        capacity: usize,
        page_size: usize,
    ) -> Result<()> {
        if self.pools.contains_key(&id) {
            return Err(Error::Catalog(format!(
                "buffer pool id {id} already exists"
            )));
        }
        let pool = BufferPool::new(name, capacity, page_size);
        log::info!(
            "created buffer pool {} (id={id}, capacity={capacity}, page_size={page_size})",
            pool.name()
        );
        self.pools.insert(id, pool);
        Ok(())
    }

    /// Get a shared reference to a pool by ID.
    pub fn get(&self, id: BufferPoolId) -> Result<&BufferPool> {
        self.pools.get(&id).ok_or_else(|| {
            Error::Catalog(format!("buffer pool id {id} not found"))
        })
    }

    /// Get a mutable reference to a pool by ID.
    pub fn get_mut(&mut self, id: BufferPoolId) -> Result<&mut BufferPool> {
        self.pools.get_mut(&id).ok_or_else(|| {
            Error::Catalog(format!("buffer pool id {id} not found"))
        })
    }

    /// Register a heap file with the specified pool.
    pub fn register_file(
        &mut self,
        pool_id: BufferPoolId,
        path: &Path,
        page_size: usize,
    ) -> Result<FileId> {
        let pool = self.pools.get_mut(&pool_id).ok_or_else(|| {
            Error::Catalog(format!("buffer pool id {pool_id} not found"))
        })?;
        pool.register_file(path, page_size)
    }

    /// Flush all dirty pages across **all** pools.
    pub fn flush_all(&mut self) -> Result<()> {
        for pool in self.pools.values_mut() {
            pool.flush_all()?;
        }
        Ok(())
    }

    /// Evict all pages for a file from the specified pool and unregister it.
    pub fn evict_file(&mut self, pool_id: BufferPoolId, file_id: FileId) -> Result<()> {
        let pool = self.pools.get_mut(&pool_id).ok_or_else(|| {
            Error::Catalog(format!("buffer pool id {pool_id} not found"))
        })?;
        pool.evict_file(file_id)
    }

    /// Return pool IDs and their names for diagnostics.
    pub fn pool_ids(&self) -> Vec<(BufferPoolId, &str)> {
        let mut ids: Vec<_> = self.pools.iter()
            .map(|(&id, p)| (id, p.name()))
            .collect();
        ids.sort_by_key(|&(id, _)| id);
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::page::{PageRead, PageWrite, SlottedPage};
    use std::fs;

    const PAGE_SIZE: usize = 256;

    /// Temp file with automatic cleanup.
    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("rqdb_pool_{name}"));
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
        let mut pool = BufferPool::new("TESTBP", 4, PAGE_SIZE);
        let (_tmp, fid) = setup_file(&mut pool, "fetch_unpin", 2);

        let page = pool.fetch_page(fid, 0).unwrap();
        assert_eq!(page.page_id(), 0);

        pool.unpin(fid, 0, false).unwrap();
    }

    #[test]
    fn fetch_same_page_twice() {
        let mut pool = BufferPool::new("TESTBP", 4, PAGE_SIZE);
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
        let mut pool = BufferPool::new("TESTBP", 4, PAGE_SIZE);
        let (_tmp, fid) = setup_file(&mut pool, "dirty_flag", 1);

        let _page = pool.fetch_page(fid, 0).unwrap();
        pool.unpin(fid, 0, true).unwrap();

        // Frame should still be dirty.
        let idx = pool.page_table[&(fid, 0)];
        assert!(pool.frame_meta[idx].dirty);
    }

    #[test]
    fn flush_page_clears_dirty() {
        let mut pool = BufferPool::new("TESTBP", 4, PAGE_SIZE);
        let (_tmp, fid) = setup_file(&mut pool, "flush_dirty", 1);

        let _page = pool.fetch_page(fid, 0).unwrap();
        pool.unpin(fid, 0, true).unwrap();
        pool.flush_page(fid, 0).unwrap();

        let idx = pool.page_table[&(fid, 0)];
        assert!(!pool.frame_meta[idx].dirty);
    }

    #[test]
    fn eviction_flushes_dirty_page() {
        // Pool with only 1 frame — fetching a second page forces eviction.
        let mut pool = BufferPool::new("TESTBP", 1, PAGE_SIZE);
        let (_tmp, fid) = setup_file(&mut pool, "evict_dirty", 2);

        // Fetch page 0, mark dirty, unpin.
        {
            let mut page = pool.fetch_page_mut(fid, 0).unwrap();
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
        let mut pool = BufferPool::new("TESTBP", 2, PAGE_SIZE);
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
        let mut pool = BufferPool::new("TESTBP", 1, PAGE_SIZE);
        let (_tmp, fid) = setup_file(&mut pool, "all_pinned", 2);

        let _p0 = pool.fetch_page(fid, 0).unwrap();
        // Don't unpin — pool is full and all pinned.

        let err = pool.fetch_page(fid, 1).unwrap_err();
        assert!(err.to_string().contains("all frames are pinned"));

        pool.unpin(fid, 0, false).unwrap();
    }

    #[test]
    fn new_page_creates_and_pins() {
        let mut pool = BufferPool::new("TESTBP", 4, PAGE_SIZE);
        let (_tmp, fid) = setup_file(&mut pool, "new_page", 0);

        let (pid, page) = pool.new_page(fid).unwrap();
        assert_eq!(pid, 0);
        assert_eq!(page.page_id(), 0);

        // Frame is dirty and pinned.
        let idx = pool.page_table[&(fid, pid)];
        assert!(pool.frame_meta[idx].dirty);
        assert_eq!(pool.frame_meta[idx].pin_count, 1);

        pool.unpin(fid, pid, true).unwrap();
    }

    #[test]
    fn flush_all_writes_all_dirty() {
        let mut pool = BufferPool::new("TESTBP", 4, PAGE_SIZE);
        let (_tmp, fid) = setup_file(&mut pool, "flush_all", 2);

        // Mark both pages dirty.
        {
            let mut p0 = pool.fetch_page_mut(fid, 0).unwrap();
            p0.insert_row(b"d0");
        }
        pool.unpin(fid, 0, true).unwrap();
        {
            let mut p1 = pool.fetch_page_mut(fid, 1).unwrap();
            p1.insert_row(b"d1");
        }
        pool.unpin(fid, 1, true).unwrap();

        pool.flush_all().unwrap();

        // Both frames should be clean now.
        for idx in 0..pool.frame_meta.len() {
            if pool.frame_meta[idx].in_use {
                assert!(!pool.frame_meta[idx].dirty);
            }
        }
    }

    #[test]
    fn multiple_files() {
        let mut pool = BufferPool::new("TESTBP", 8, PAGE_SIZE);
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
        let mut pool = BufferPool::new("TESTBP", 4, PAGE_SIZE);
        let (_tmp, fid) = setup_file(&mut pool, "unpin_twice", 1);

        let _p = pool.fetch_page(fid, 0).unwrap();
        pool.unpin(fid, 0, false).unwrap();

        // Second unpin should fail (pin_count already 0).
        let err = pool.unpin(fid, 0, false).unwrap_err();
        assert!(err.to_string().contains("unpinned"));
    }

    #[test]
    fn fetch_mut_marks_dirty() {
        let mut pool = BufferPool::new("TESTBP", 4, PAGE_SIZE);
        let (_tmp, fid) = setup_file(&mut pool, "fetch_mut", 1);

        let _page = pool.fetch_page_mut(fid, 0).unwrap();
        let idx = pool.page_table[&(fid, 0)];
        assert!(pool.frame_meta[idx].dirty);
        pool.unpin(fid, 0, false).unwrap();
    }

    #[test]
    fn page_size_mismatch_rejected() {
        let mut pool = BufferPool::new("TESTBP", 4, PAGE_SIZE);
        let tmp = TempFile::new("mismatch");
        let err = pool.register_file(&tmp.path, PAGE_SIZE * 2).unwrap_err();
        assert!(err.to_string().contains("page size mismatch"));
    }

    #[test]
    fn pre_allocated_capacity() {
        let pool = BufferPool::new("TESTBP", 16, PAGE_SIZE);
        // The entire region is allocated upfront.
        assert_eq!(pool.pool_buf.len(), 16 * PAGE_SIZE);
        assert_eq!(pool.capacity(), 16);
        assert_eq!(pool.page_size(), PAGE_SIZE);
        assert_eq!(pool.name(), "TESTBP");
    }

    #[test]
    fn manager_create_multiple_pools() {
        let mut mgr = BufferPoolManager::new();
        mgr.create_pool(1, "RQDEFAULTBP", 64, PAGE_SIZE).unwrap();
        mgr.create_pool(2, "INDEXBP", 32, PAGE_SIZE).unwrap();
        mgr.create_pool(3, "LOBBP", 16, 8192).unwrap();

        assert_eq!(mgr.get(1).unwrap().name(), "RQDEFAULTBP");
        assert_eq!(mgr.get(2).unwrap().capacity(), 32);
        assert_eq!(mgr.get(3).unwrap().page_size(), 8192);

        let ids = mgr.pool_ids();
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0], (1, "RQDEFAULTBP"));
    }

    #[test]
    fn manager_duplicate_pool_id_rejected() {
        let mut mgr = BufferPoolManager::new();
        mgr.create_pool(1, "BP1", 4, PAGE_SIZE).unwrap();
        let err = mgr.create_pool(1, "BP2", 4, PAGE_SIZE).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn manager_register_file_routes_to_pool() {
        let mut mgr = BufferPoolManager::new();
        mgr.create_pool(1, "DATABP", 8, PAGE_SIZE).unwrap();
        mgr.create_pool(2, "INDEXBP", 8, PAGE_SIZE).unwrap();

        let tmp1 = TempFile::new("mgr_data");
        let tmp2 = TempFile::new("mgr_idx");

        let fid1 = mgr.register_file(1, &tmp1.path, PAGE_SIZE).unwrap();
        let fid2 = mgr.register_file(2, &tmp2.path, PAGE_SIZE).unwrap();

        // Both get file_id 0 since they're in different pools.
        assert_eq!(fid1, 0);
        assert_eq!(fid2, 0);
    }

    #[test]
    fn manager_flush_all_pools() {
        let mut mgr = BufferPoolManager::new();
        mgr.create_pool(1, "BP1", 4, PAGE_SIZE).unwrap();
        mgr.create_pool(2, "BP2", 4, PAGE_SIZE).unwrap();
        // flush_all should succeed even with empty pools.
        mgr.flush_all().unwrap();
    }

    // ── Latch enforcement tests ──

    #[test]
    fn exclusive_latch_blocks_shared_read() {
        let mut pool = BufferPool::new("TESTBP", 4, PAGE_SIZE);
        let (_tmp, fid) = setup_file(&mut pool, "excl_blocks_read", 1);

        // Acquire exclusive write latch.
        let _page = pool.fetch_page_mut(fid, 0).unwrap();

        // Shared read on the same page must fail.
        let err = pool.fetch_page(fid, 0).unwrap_err();
        assert!(err.to_string().contains("exclusively latched"));

        pool.unpin(fid, 0, false).unwrap();
    }

    #[test]
    fn shared_read_blocks_exclusive_write() {
        let mut pool = BufferPool::new("TESTBP", 4, PAGE_SIZE);
        let (_tmp, fid) = setup_file(&mut pool, "read_blocks_excl", 1);

        // Acquire shared read latch.
        let _page = pool.fetch_page(fid, 0).unwrap();

        // Exclusive write on the same page must fail.
        let err = pool.fetch_page_mut(fid, 0).unwrap_err();
        assert!(err.to_string().contains("exclusive write latch"));

        pool.unpin(fid, 0, false).unwrap();
    }

    #[test]
    fn shared_read_allows_multiple_readers() {
        let mut pool = BufferPool::new("TESTBP", 4, PAGE_SIZE);
        let (_tmp, fid) = setup_file(&mut pool, "multi_readers", 1);

        // Two shared readers on the same page should succeed.
        let _p1 = pool.fetch_page(fid, 0).unwrap();
        pool.unpin(fid, 0, false).unwrap();
        let _p2 = pool.fetch_page(fid, 0).unwrap();
        pool.unpin(fid, 0, false).unwrap();

        // Frame pin_count back to 0, latch cleared.
        let idx = pool.page_table[&(fid, 0)];
        assert_eq!(pool.frame_meta[idx].latch, LatchMode::None);
    }

    #[test]
    fn latch_cleared_after_unpin() {
        let mut pool = BufferPool::new("TESTBP", 4, PAGE_SIZE);
        let (_tmp, fid) = setup_file(&mut pool, "latch_clear", 1);

        // Exclusive latch then unpin.
        let _page = pool.fetch_page_mut(fid, 0).unwrap();
        pool.unpin(fid, 0, false).unwrap();

        let idx = pool.page_table[&(fid, 0)];
        assert_eq!(pool.frame_meta[idx].latch, LatchMode::None);

        // Should now be able to acquire shared read.
        let _page = pool.fetch_page(fid, 0).unwrap();
        pool.unpin(fid, 0, false).unwrap();
    }

    #[test]
    fn new_page_acquires_exclusive_latch() {
        let mut pool = BufferPool::new("TESTBP", 4, PAGE_SIZE);
        let (_tmp, fid) = setup_file(&mut pool, "new_page_latch", 0);

        let (pid, _page) = pool.new_page(fid).unwrap();
        let idx = pool.page_table[&(fid, pid)];
        assert_eq!(pool.frame_meta[idx].latch, LatchMode::Exclusive);

        pool.unpin(fid, pid, true).unwrap();
        assert_eq!(pool.frame_meta[idx].latch, LatchMode::None);
    }
}
