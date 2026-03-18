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

**Design:**
- Fixed-size pool of `N` page frames (configurable, default 128 or any size)
- **LRU replacement policy** — `VecDeque<FrameIndex>` where front = oldest
- **Dirty-page tracking** — frames carry a `dirty: bool` flag; dirty pages
  are flushed to disk before eviction (lazy/async flush model)
- **Pin count** — pages in active use are pinned; pinned frames cannot be
  evicted. A frame re-enters the LRU list only when `pin_count` drops to 0.
- **File registration** — `register_file(path, page_size) -> FileId` maps
  arbitrary `.DAT` files into the pool by ID

### Buffer Pool API

| Method | Signature | Description |
|--------|-----------|-------------|
| `new` | `(capacity) -> Self` | Create pool with N frames |
| `register_file` | `(&mut self, path, page_size) -> Result<FileId>` | Register a heap file |
| `fetch_page` | `(&mut self, FileId, PageId) -> Result<&SlottedPage>` | Pin & return read-only ref |
| `fetch_page_mut` | `(&mut self, FileId, PageId) -> Result<&mut SlottedPage>` | Pin & return mutable ref (auto-dirty) |
| `new_page` | `(&mut self, FileId) -> Result<(PageId, &mut SlottedPage)>` | Allocate new page, pinned + dirty |
| `unpin` | `(&mut self, FileId, PageId, dirty: bool) -> Result<()>` | Decrement pin, optionally mark dirty |
| `flush_page` | `(&mut self, FileId, PageId) -> Result<()>` | Write dirty page to disk |
| `flush_all` | `(&mut self) -> Result<()>` | Flush all dirty pages |
| `used_frames` | `(&self) -> usize` | Count of occupied frames |
| `capacity` | `(&self) -> usize` | Total frame count |

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

### Tests (12)

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

### TODO: Convert to Pre-Allocated Pool

The current implementation allocates page buffers on-demand (`Vec<u8>` per
`SlottedPage`). Real database engines (DB2, PostgreSQL, MySQL) use a single
contiguous pre-allocated memory region. Benefits of converting:

- **Predictable memory** — claim all memory at startup, no surprise OOM
- **Zero allocation in hot path** — page fetch is a `memcpy` into a fixed slice
- **Cache/TLB locality** — one contiguous region vs scattered heap allocations
- **No fragmentation** — avoid alloc/free churn from thousands of page loads
- **Recovery safety** — fixed memory budget, pool never grows

Target design: allocate `capacity × page_size` bytes upfront, each frame owns
a fixed slice, `SlottedPage` borrows the slice instead of owning a `Vec<u8>`.

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
