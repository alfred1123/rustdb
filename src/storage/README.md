# storage/

Page-based storage engine.

## Files

| File      | Purpose                                         |
|-----------|-------------------------------------------------|
| `page.rs` | Slotted page: 24-byte header, slot directory, row data, CRC32 checksums |
| `heap.rs` | Heap file: manages a `.DAT` file as a sequence of slotted pages, RID addressing |
| `pool.rs` | Buffer pool: fixed-size frame pool, LRU eviction, dirty-page tracking, pin counts |

---

## Phase 1 — Slotted Page (`page.rs`) ✅

A `SlottedPage` wraps a single fixed-size `Vec<u8>` buffer. All header fields
are stored inline at known byte offsets — there are no separate Rust struct
fields. This means the in-memory layout matches the on-disk format exactly,
so reading/writing is a direct copy with no serialization step.

### Page Layout

```
┌─────────────────────────────────────────────┐
│ Page Header (24 bytes)                      │
│   page_id       : u64     (bytes 0–7)      │
│   page_type     : u8      (byte  8)        │
│   free_space    : u16     (bytes 9–10)     │
│   slot_count    : u16     (bytes 11–12)    │
│   checksum      : u32     (bytes 13–16)    │
│   reserved      : 7 bytes (bytes 17–23)    │
├─────────────────────────────────────────────┤
│ Slot Directory (grows → from byte 24)       │
│   slot 0: offset u16 + length u16 (4 B)    │
│   slot 1: offset u16 + length u16          │
│   ...                                      │
├─────────────────────────────────────────────┤
│            ↓ free space ↓                   │
├─────────────────────────────────────────────┤
│ Row Data (grows ← from end of page)         │
│   row N bytes...                            │
│   row N-1 bytes...                          │
│   ...                                      │
└─────────────────────────────────────────────┘
```

All multi-byte values are **little-endian**.

### Page Types

| Value | Constant         | Meaning       |
|-------|------------------|---------------|
| `0`   | `PAGE_TYPE_FREE` | Free / unused |
| `1`   | `PAGE_TYPE_DATA` | Data (heap)   |

### Slot Directory

Each slot is 4 bytes: `u16 offset` + `u16 length`.
- A valid slot points to row data within the page.
- A **deleted** slot is marked as `(offset=0, length=0)` — a tombstone.
  The row bytes are **not** physically removed; space is reclaimed only
  by a future compaction/reorg (not yet implemented). This supports
  MVCC snapshots, transaction rollback, and simple WAL logging.
- On insert, deleted slots are scanned and **reused** before appending
  a new slot entry.

### CRC32 Checksum

Integrity is verified via `crc32fast`. The checksum covers all page bytes
**except** the 4-byte checksum field itself (bytes 13–16), avoiding
circular dependency.

- **Write path:** Every mutation (`insert_row`, `delete_row`, `new`)
  recomputes and stores the CRC.
- **Read path:** `from_bytes()` recomputes the CRC and compares it to
  the stored value. Mismatch → `Error::Corruption`.

### Public API

| Method | Signature | Description |
|--------|-----------|-------------|
| `new` | `(page_id, page_size) -> Self` | Create empty data page |
| `from_bytes` | `(Vec<u8>) -> Result<Self>` | Load from disk, verify checksum |
| `insert_row` | `(&mut self, &[u8]) -> Option<SlotIndex>` | Insert row, `None` if page full |
| `read_row` | `(&self, SlotIndex) -> Option<&[u8]>` | Read row bytes, `None` if deleted/OOB |
| `delete_row` | `(&mut self, SlotIndex) -> bool` | Tombstone a slot, `false` if already gone |
| `free_space` | `(&self) -> usize` | Usable bytes remaining (accounts for new slot entry) |
| `as_bytes` | `(&self) -> &[u8]` | Raw buffer for writing to disk |
| `into_bytes` | `(self) -> Vec<u8>` | Consume and return buffer |

### Tests (10)

- Header correctness, insert/read, multi-row insert, delete, slot reuse,
  overflow rejection, fill-until-full, byte roundtrip, checksum corruption
  detection, out-of-range slot access.

---

## Phase 2 — Heap File (`heap.rs`) ✅

A `HeapFile` manages a single `.DAT` file as an ordered sequence of slotted
pages. Each table maps to one heap file. Rows are addressed by **RID**
(Record Identifier).

### RID (Record Identifier)

```rust
pub struct Rid {
    pub page_id: PageId,   // u64 — which page
    pub slot: SlotIndex,   // u16 — which slot within the page
}
```

A RID uniquely identifies a row within a table's heap file. This is the
physical address used by the buffer pool and future index layer.

### File Layout

```
┌──────────────┬──────────────┬──────────────┬─────┐
│   Page 0     │   Page 1     │   Page 2     │ ... │
│ (page_size B)│ (page_size B)│ (page_size B)│     │
└──────────────┴──────────────┴──────────────┴─────┘
```

Page N lives at file offset `N * page_size`. Pages are read/written
individually via seeking.

### Free-Space Map

An in-memory `Vec<bool>` tracks which pages may have room for inserts:

| Event | Effect |
|-------|--------|
| File opened | All existing pages marked `true` (optimistic) |
| After `write_page` | Updated based on `page.free_space() > 0` |
| Insert fails on a page | Marked `false` (full) |
| Row deleted from a page | Marked `true` (space freed) |

On insert, pages marked `false` are skipped. This avoids reading every page
from disk to check for space. (Can be upgraded to an on-disk bitmap later.)

### Public API

| Method | Signature | Description |
|--------|-----------|-------------|
| `open` | `(path, page_size) -> Result<Self>` | Open or create a `.DAT` file |
| `read_page` | `(&mut self, PageId) -> Result<SlottedPage>` | Read page from disk |
| `write_page` | `(&mut self, &SlottedPage) -> Result<()>` | Write page to disk |
| `read_page_into` | `(&mut self, PageId, &mut [u8]) -> Result<()>` | Read page into caller buffer (zero-alloc) |
| `write_page_buf` | `(&mut self, &[u8]) -> Result<()>` | Write raw page bytes to disk |
| `insert_row` | `(&mut self, &[u8]) -> Result<Rid>` | Find/create page, insert row |
| `read_row` | `(&mut self, Rid) -> Result<Vec<u8>>` | Fetch row by RID |
| `delete_row` | `(&mut self, Rid) -> Result<bool>` | Tombstone a row |
| `scan` | `(&mut self) -> Result<Vec<(Rid, Vec<u8>)>>` | All live rows across all pages |
| `page_count` | `(&self) -> u64` | Number of pages in the file |
| `page_size` | `(&self) -> usize` | Page size for this heap |

### Insert Flow

1. Scan `free_map` for a page marked `true`.
2. Read that page, attempt `page.insert_row(row)`.
3. If it fits → write page back, return RID.
4. If not → mark page `false`, try next.
5. If no existing page has space → create new page, insert, append to file.
6. If row exceeds a single page's capacity → return error.

### Tests (8)

- Empty heap creation, insert/read, multi-row same page, page spill,
  delete + scan, persistence across reopen, empty scan, oversized row
  rejection.

---

## Remaining Phases (Planned)

### Phase 3 — Buffer Pool (`pool.rs`) ✅

The buffer pool sits between heap files and the rest of the engine. All page
reads/writes go through it. No component above the buffer pool touches disk
directly.

**Design — Pre-Allocated Contiguous Pool:**

All `capacity × page_size` bytes are allocated **once** at construction in a
single contiguous `Vec<u8>`. Each frame owns a fixed slice of this region —
no per-page heap allocation occurs on the fetch/evict hot path.

- **Fixed page size per pool** — `BufferPool::new("name", capacity, page_size)`.
  All registered files must match the pool's page size (DB2-style).
- **Named pools** — Each `BufferPool` carries a name (e.g., `"RQDEFAULTBP"`)
  for diagnostics and catalog correlation.
- **One contiguous memory region** — `pool_buf: Vec<u8>` of
  `capacity * page_size` bytes. Frame `i` occupies
  `pool_buf[i*page_size .. (i+1)*page_size]`.
- **Metadata-only frames** — `FrameMeta` tracks `file_id`, `page_id`,
  `pin_count`, `dirty`, `in_use`. No per-frame `SlottedPage` or `Vec<u8>`.
- **LRU replacement policy** — `VecDeque<FrameIndex>` where front = oldest.
- **Dirty-page tracking** — frames carry a `dirty: bool` flag; dirty pages
  are flushed to disk before eviction (lazy flush model).
- **Pin count** — pages in active use are pinned; pinned frames cannot be
  evicted. A frame re-enters the LRU list only when `pin_count` drops to 0.
- **File registration** — `register_file(path, page_size) -> FileId` maps
  `.DAT` files into the pool. Page size is validated against the pool's size.

**Zero-allocation I/O path:**

On `fetch_page`, data is read from disk directly into the pre-allocated frame
slice via `HeapFile::read_page_into()` — no temporary `Vec<u8>` allocation.
Checksum verification runs on the in-place data. On flush, frame data is
written back via `HeapFile::write_page_buf()`.

**Borrowed page views (`PageRef` / `PageMut`):**

The pool returns lightweight view types instead of owned `SlottedPage`:
- `PageRef<'a>` — read-only view wrapping `&'a [u8]` from the pool buffer
- `PageMut<'a>` — mutable view wrapping `&'a mut [u8]` from the pool buffer

Both types implement the `PageRead` / `PageWrite` traits (defined in
`page.rs`) via shared free functions — no logic duplication. `SlottedPage`
(owned `Vec<u8>`) continues to be used by `HeapFile` for standalone I/O.

### Buffer Pool API

| Method | Signature | Description |
|--------|-----------|-------------|
| `new` | `(name, capacity, page_size) -> Self` | Pre-allocate named pool with N frames |
| `name` | `(&self) -> &str` | Pool name for diagnostics |
| `register_file` | `(&mut self, path, page_size) -> Result<FileId>` | Register a heap file (page size validated) |
| `fetch_page` | `(&mut self, FileId, PageId) -> Result<PageRef<'_>>` | Pin & return read-only view |
| `fetch_page_mut` | `(&mut self, FileId, PageId) -> Result<PageMut<'_>>` | Pin & return mutable view (auto-dirty) |
| `new_page` | `(&mut self, FileId) -> Result<(PageId, PageMut<'_>)>` | Allocate new page, pinned + dirty |
| `unpin` | `(&mut self, FileId, PageId, dirty: bool) -> Result<()>` | Decrement pin, optionally mark dirty |
| `flush_page` | `(&mut self, FileId, PageId) -> Result<()>` | Write dirty page to disk |
| `flush_all` | `(&mut self) -> Result<()>` | Flush all dirty pages |
| `used_frames` | `(&self) -> usize` | Count of occupied frames |
| `capacity` | `(&self) -> usize` | Total frame count |
| `page_size` | `(&self) -> usize` | Fixed page size for this pool |

### Page Trait Hierarchy (`page.rs`)

Core page operations are implemented as free functions on `&[u8]` / `&mut [u8]`
slices. Three concrete types delegate to them via traits:

```
PageRead (trait)          PageWrite (trait: PageRead)
├─ SlottedPage (owned)    ├─ SlottedPage
├─ PageRef<'a> (borrowed) └─ PageMut<'a> (borrowed)
└─ PageMut<'a>
```

| Trait | Methods |
|-------|---------|
| `PageRead` | `page_id`, `page_type`, `slot_count`, `page_size`, `free_space`, `read_row`, `as_bytes` |
| `PageWrite` | `insert_row`, `delete_row` |

### Dirty Page Flush Model

Pages are **not** flushed synchronously on every write. Instead:
1. Mutations mark the frame `dirty = true`.
2. Dirty pages are flushed to disk **lazily** — either:
   - On **eviction**: when an LRU victim is dirty, it is flushed before reuse.
   - On **explicit flush**: `flush_page()` or `flush_all()` for checkpoint ops.
3. This deferred-write model reduces I/O for write-heavy workloads and aligns
   with the WAL contract: the WAL record is written before the data page (later).

### Eviction Flow

1. Look for a free frame (no page loaded).
2. If none, pop the **front** of the LRU deque (oldest unpinned frame).
3. If that frame is dirty → flush to disk first.
4. Remove old page-table entry, reset frame, return for reuse.
5. If the LRU deque is empty (all frames pinned) → return error.

### Tests (14)

- `fetch_and_unpin` — basic fetch + unpin lifecycle
- `fetch_same_page_twice` — pool hit returns same frame
- `dirty_flag_preserved_across_unpin` — dirty survives unpin
- `flush_page_clears_dirty` — explicit flush clears flag
- `eviction_flushes_dirty_page` — dirty victim flushed before reuse
- `lru_evicts_oldest_unpinned` — re-access reorders LRU
- `all_pinned_returns_error` — pool full + all pinned = error
- `new_page_creates_and_pins` — allocate new page in pool
- `flush_all_writes_all_dirty` — batch flush
- `multiple_files` — separate files coexist in pool
- `unpin_twice_errors` — double-unpin caught
- `fetch_mut_marks_dirty` — mutable fetch auto-dirties
- `page_size_mismatch_rejected` — file with wrong page size rejected
- `pre_allocated_capacity` — verifies upfront allocation size and name

### BufferPoolManager

A `BufferPoolManager` centralises multiple `BufferPool` instances in a single
`HashMap<BufferPoolId, BufferPool>`, allowing different workloads to use
independent pools with potentially different page sizes:

| Pool (default) | Purpose | Page Size |
|----------------|---------|-----------|
| `RQDEFAULTBP` (id=1) | Regular data | 4 KB |
| `INDEXBP` (id=2) | Index pages | 4 KB |
| `LOBBP` (id=3) | Large-object data | 32 KB |
| `TEMPBP` (id=4) | Temporary/sort scratch | 4 KB |

Each tablespace in `SYSTABLESPACES` references a `BUFFERPOOLID` that maps to
one of these pools. At startup the engine creates pools from `SYSBUFFERPOOLS`
catalog rows, then routes all file registrations to the correct pool.

### BufferPoolManager API

| Method | Signature | Description |
|--------|-----------|-------------|
| `new` | `() -> Self` | Create empty manager |
| `create_pool` | `(&mut self, id, name, capacity, page_size) -> Result<()>` | Add a new pool |
| `get` | `(&self, id) -> Result<&BufferPool>` | Borrow pool by ID |
| `get_mut` | `(&mut self, id) -> Result<&mut BufferPool>` | Mutable borrow by ID |
| `register_file` | `(&mut self, pool_id, path, page_size) -> Result<FileId>` | Register file in a specific pool |
| `flush_all` | `(&mut self) -> Result<()>` | Flush every dirty page in every pool |
| `pool_ids` | `(&self) -> Vec<(BufferPoolId, &str)>` | List (id, name) pairs sorted by id |

### BufferPoolManager Tests (4)

- `manager_create_multiple_pools` — creates 3 pools, validates name/capacity/page_size
- `manager_duplicate_pool_id_rejected` — duplicate pool ID returns error
- `manager_register_file_routes_to_pool` — files registered to different pools
- `manager_flush_all_pools` — flush_all succeeds across empty pools

### Phase 4 — Tablespace Manager (`tablespace.rs`)

Central coordinator that maps tablespace IDs + table names to heap files and
routes I/O through the buffer pool.

**Responsibilities:**
- On startup, open heap files for all tables listed in `SYSTABLES`
- Resolve `(schema, table_name)` → `HeapFile` using catalog metadata
- Provide a `table_scan(schema, table)` that returns an iterator of raw row
  bytes (via buffer pool → heap file → slotted pages)
- Provide `insert_row(schema, table, row_bytes) -> RID`
- Provide `delete_row(schema, table, rid)`

**Deliverables:**
- `TablespaceManager` struct owning `BufferPool` + map of open `HeapFile`s
- Methods listed above
- Integration test: create table file, insert rows, scan back

### Phase 5 — Migrate Catalog to Page-Based Storage

Once phases 1–4 are solid, migrate the system catalog tables from the current
flat row format to page-structured `.DAT` files. This means:

- Bootstrap writes catalog rows into slotted pages instead of flat streams
- Loader reads catalog via `TablespaceManager.table_scan()` instead of
  `read_binary_rows()`
- The catalog becomes truly self-describing: same storage path as user tables

**Text mode:** Text mode (`--text-mode`) remains available for debugging.
When `text_mode=true`, bypass the page layer and continue using flat TSV
files. The page-based path is the `text_mode=false` default.

### Phase 6 — Wire Up to SQL Executor

Replace the hardcoded `load_table_data()` in `executor.rs` with calls to
`TablespaceManager`:

- `load_table_data()` calls `table_scan()` to get raw row bytes
- Deserializes each row using `RowReader` + column metadata from `SYSCOLUMNS`
- Returns `(Vec<String>, Vec<Vec<Value>>)` as today

This makes `SELECT` work against any table — catalog or user — without
per-table match arms.

## Dependency Order

```
Phase 1 (page.rs)        — no dependencies
Phase 2 (heap.rs)        — depends on Phase 1
Phase 3 (pool.rs)        — depends on Phase 1, Phase 2
Phase 4 (tablespace.rs)  — depends on Phase 1–3, catalog types
Phase 5 (migrate catalog)— depends on Phase 4, bootstrap, loader
Phase 6 (executor wiring)— depends on Phase 4–5, sql/executor
```

## New Dependencies

| Crate       | Phase | Purpose          |
|-------------|-------|------------------|
| `crc32fast` | 1     | Page checksums   |
