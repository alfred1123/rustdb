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

**Planned — Current-page hint:**
The current linear scan of `free_map` reads pages one-by-one until it finds
space. A better approach is to cache the **last-insert page ID** per heap file
so the next insert jumps directly to the page most likely to have room. This
hint would be maintained by `HeapFile` (or by the tablespace manager) and
reset when the page fills. For catalog tables that are append-heavy at
bootstrap, this avoids re-scanning from page 0 on every row. See the
*Catalog Cache Strategy* section below for the broader caching plan.

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
  `pin_count`, `dirty`, `in_use`, `latch`. No per-frame `SlottedPage` or `Vec<u8>`.
- **LRU replacement policy** — `VecDeque<FrameIndex>` where front = oldest.
- **Dirty-page tracking** — frames carry a `dirty: bool` flag; dirty pages
  are flushed to disk before eviction (lazy flush model).
- **Pin count** — pages in active use are pinned; pinned frames cannot be
  evicted. A frame re-enters the LRU list only when `pin_count` drops to 0.
- **Frame latch (readers–writer)** — each frame carries a `LatchMode` that
  enforces strict ACID isolation at the buffer-pool level. See *Frame Latch*
  section below.
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
2. Dirty pages are flushed to disk **lazily** via three triggers:
   - **Eviction** (`evict_for_frame`): when an LRU victim is dirty, it is
     flushed before the frame is reused for another page.
   - **Explicit single flush** (`flush_page`): caller specifies a
     `(file_id, page_id)` to write one dirty page to disk immediately.
   - **Batch flush** (`flush_all`): iterates all frames and flushes every
     dirty page — used for checkpoint operations.
3. All three paths delegate to the internal `flush_frame(idx)` helper, which
   writes the frame's slice to disk via `HeapFile::write_page_buf()` and
   clears the `dirty` flag.
4. This deferred-write model reduces I/O for write-heavy workloads and aligns
   with the WAL contract: the WAL record is written before the data page (later).

### Page Load Path (Disk I/O)

The actual disk read happens inside the `ensure_loaded` internal helper.
When a page is not already in the pool (cache miss), this is the path:

```
fetch_page / fetch_page_mut
  │
  └─► ensure_loaded(file_id, page_id)
        │
        ├─ Fast path: page_table lookup hit → return frame index (no I/O)
        │
        └─ Slow path: cache miss
             │
             ├─ evict_for_frame()  → find/evict a frame
             │    ├─ free frame available → use it
             │    └─ no free frame → pop LRU front (flush if dirty)
             │
             ├─ heap.read_page_into(page_id, frame_buf)
             │    └─ actual disk read into pre-allocated frame slice
             │
             ├─ verify_checksum_of(frame_buf)
             │    └─ CRC32 integrity check on in-place data
             │
             └─ update FrameMeta + page_table → return frame index
```

The disk write path mirrors this: `flush_frame(idx)` writes the frame's
slice back to disk via `heap.write_page_buf()` and clears the dirty flag.

### Internal Helpers

| Helper | Purpose |
|--------|---------|
| `ensure_loaded(file_id, page_id)` | Guarantee page is in a frame; load from disk on cache miss |
| `evict_for_frame()` | Find a free or evictable frame; flush dirty victim before reuse |
| `flush_frame(idx)` | Write a single frame to its heap file; clear dirty flag |

These are private to `BufferPool` — all external access goes through the
public API (`fetch_page`, `fetch_page_mut`, `new_page`, `unpin`, `flush_*`).

### Eviction Flow

1. Look for a free frame (no page loaded).
2. If none, pop the **front** of the LRU deque (oldest unpinned frame).
3. If that frame is dirty → flush to disk first.
4. Remove old page-table entry, reset frame (including latch), return for reuse.
5. If the LRU deque is empty (all frames pinned) → return error.

### Frame Latch (Readers–Writer Exclusion)

Each frame carries a `LatchMode` that enforces **strict ACID isolation** at
the buffer-pool level. This prevents concurrent read/write conflicts on the
same page — no uncommitted reads are possible.

```rust
enum LatchMode {
    None,       // frame idle — no active pins
    Shared,     // one or more readers hold the frame
    Exclusive,  // exactly one writer holds the frame
}
```

**Latch rules:**

| Existing latch | `fetch_page` (shared read) | `fetch_page_mut` (exclusive write) |
|---|---|---|
| `None` | ✅ Allowed → `Shared` | ✅ Allowed → `Exclusive` |
| `Shared` (readers active) | ✅ Allowed (pin_count++) | ❌ Rejected — readers active |
| `Exclusive` (writer active) | ❌ Rejected — writer active | ❌ Rejected — writer active |

- `new_page` always acquires `Exclusive` (the page is freshly created + dirty).
- `unpin` decrements `pin_count`; when it reaches 0 the latch resets to `None`
  and the frame re-enters the LRU list.
- Eviction resets the latch to `None` as part of clearing the frame.

**Design note — future uncommitted-read support:**
The current model is strict (serializable-level page access). To support
`READ UNCOMMITTED` isolation later, a new code path could allow `fetch_page`
when `latch == Exclusive`, returning a read-only view of the in-progress
dirty page. This was intentionally deferred — the latch enum and check
structure are designed to make that addition a localised change.

### Tests (19)

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
- `exclusive_latch_blocks_shared_read` — write latch rejects readers
- `shared_read_blocks_exclusive_write` — read latch rejects writers
- `shared_read_allows_multiple_readers` — multiple shared readers coexist
- `latch_cleared_after_unpin` — latch resets to None on full unpin
- `new_page_acquires_exclusive_latch` — new page starts with exclusive latch

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

## Catalog Cache Strategy

Catalog tables (`SYSTABLES`, `SYSCOLUMNS`, `SYSTABLESPACES`, `SYSSCHEMAS`,
`SYSBUFFERPOOLS`) are read on almost every SQL operation — query planning
needs column metadata, the executor needs tablespace-to-file mappings, etc.
Reading them from disk each time is wasteful. The strategy below keeps
catalog data in memory for fast access while preserving correctness.

### Design

1. **Eager load at startup.** During database open the catalog loader reads
   all catalog tables once and materializes them into in-memory structures
   (e.g., `HashMap<(Schema, TableName), TableMeta>`). This is the
   authoritative cache for the lifetime of the process.

2. **Write-through on DDL.** When a DDL statement (`CREATE TABLE`,
   `ALTER TABLE`, `DROP TABLE`, etc.) mutates a catalog table, the change
   is written to disk **and** applied to the in-memory cache in the same
   operation. No stale reads.

3. **Per-heap current-page hint.** Each cached catalog entry for a heap file
   stores a `last_insert_page: Option<PageId>` — the last page known to
   have free space. `insert_row` tries this page first before falling back
   to the `free_map` scan. The hint is updated after each insert and
   cleared when the page fills.

4. **Column metadata cache.** Column definitions from `SYSCOLUMNS` are
   grouped by `(schema, table)` and cached as `Vec<ColumnDef>`. The SQL
   planner and `RowReader` read from this cache — zero disk I/O for column
   lookups after startup.

5. **Tablespace → buffer pool routing cache.** The mapping
   `tbspaceid → BufferPoolId` (from `SYSTABLESPACES`) is cached so that
   file registrations and page fetches resolve the correct pool without
   re-reading catalog rows.

### Cache Invalidation Rules

| Event | Action |
|-------|--------|
| Startup / bootstrap | Full load from disk into cache |
| `CREATE TABLE` | Insert into `SYSTABLES` + `SYSCOLUMNS` on disk, add to cache |
| `DROP TABLE` | Delete from disk, remove from cache |
| `ALTER TABLE ADD COLUMN` | Update disk rows, append to cached column list |
| `CREATE TABLESPACE` | Insert into `SYSTABLESPACES` on disk, add to routing cache |
| Buffer pool eviction | No cache impact — cache is separate from page frames |

Because RustDB is currently single-session, there is no cross-session
invalidation concern. When multi-session support is added, the cache
will need a latch or read-copy-update (RCU) scheme.

### Dependency

The catalog cache sits between the **catalog loader** and the **SQL executor /
tablespace manager**. It does not replace the buffer pool — catalog *pages*
still flow through the buffer pool for I/O; the cache holds *deserialized*
rows for fast lookup.

```
SQL executor / planner
        │
        ▼
  Catalog Cache  (in-memory HashMap of deserialized catalog rows)
        │  (miss on startup only — full eager load)
        ▼
  Catalog Loader  (reads raw row bytes via buffer pool)
        │
        ▼
  Buffer Pool → Heap File → Disk
```

## Future Development Options

RustDB follows DB2-style conventions. The table below compares the current
approach with Oracle-style alternatives that could be adopted if workload
demands justify the added complexity.

### Buffer Pool: Named Pools vs Shared Cache

| | Current (DB2-style) | Alternative (Oracle-style) |
|---|---|---|
| **Design** | Per-tablespace named pools (`RQDEFAULTBP`, `INDEXBP`, …) | Single shared buffer cache with optional `KEEP`/`RECYCLE` sub-pools |
| **Pro** | Workload isolation — catalog pages can't evict hot user data | Auto-adapts to shifting workloads without manual sizing |
| **Con** | Must size pools upfront; idle memory in one pool can't help another | Less predictable per-workload performance; more complex eviction logic |

**Potential upgrade:** Add a dynamic rebalancing layer that can shrink idle
pools and grow busy ones at runtime, getting the isolation benefits with
better memory utilisation.

### Eviction Policy: LRU vs Alternative Mechanisms

RustDB currently uses strict LRU via `VecDeque<FrameIndex>`. LRU is the
industry-standard default, but production databases adapt it to specific
workload patterns. The table below compares alternatives that could be
adopted if profiling reveals LRU limitations.

| Mechanism | Used By | Situation | Why Used |
|-----------|---------|-----------|----------|
| **Strict LRU** | SQLite, RustDB (current) | Small to medium pools, embedded databases | Simplest to implement; good default for general workloads |
| **CLOCK (circular LRU)** | PostgreSQL | Large buffer pools with high throughput | Approximates LRU with O(1) eviction — avoids moving entries in a linked list on every access; uses a reference bit swept by a clock hand |
| **LRU-K** | Microsoft SQL Server | Mixed OLTP/OLAP with repeated sequential scans | Tracks the last K accesses per page; a single sequential scan doesn't pollute the cache because pages need multiple hits to become "hot" |
| **Midpoint Insertion (Young/Old LRU)** | MySQL InnoDB | Full-table scans mixed with point lookups | New pages enter at the midpoint (3/8 from tail); only pages re-accessed after a configurable interval promote to the "young" head — prevents scan floods from evicting hot pages |
| **Touch Count + Hot/Cold Lists** | Oracle DB | High-concurrency OLTP with many concurrent sessions | Tracks touch count per buffer; splits cache into hot and cold ends; avoids LRU list contention under thousands of concurrent latches |
| **MRU (Most Recently Used)** | IBM DB2 (configurable) | Large sequential scans (e.g., `FETCH FIRST` over a massive table) | After a full scan the *most* recently read pages are least likely to be reused — evicting them first keeps earlier (potentially re-scanned) pages resident |
| **LFU (Least Frequently Used)** | Rare; research systems, some caching layers | Stable hot-set workloads with long-lived popular pages | Evicts the least-accessed page overall; excellent when the hot set is small and stable, but slow to adapt when access patterns shift |
| **ARC (Adaptive Replacement Cache)** | ZFS, IBM DS8000 | Workloads that shift between recency-friendly and frequency-friendly patterns | Self-tuning hybrid of LRU and LFU; dynamically adjusts the split between recent-once and recent-many lists without manual configuration |
| **2Q (Two-Queue)** | Research, some storage engines | Scan-resistant caching with minimal tuning | Incoming pages go to a short FIFO queue; only pages re-accessed within the FIFO window promote to a main LRU queue — cheap scan resistance |

**Potential upgrade path for RustDB:**

1. **Near-term — CLOCK sweep.** Replace the `VecDeque` LRU with a circular
   buffer + reference bit. This eliminates the O(n) `retain()` calls on
   every `fetch_page` / `fetch_page_mut` while preserving LRU-like behavior.
   Minimal API change — only internal eviction logic changes.

2. **Medium-term — Midpoint insertion.** Split the LRU deque into young/old
   regions (configurable ratio, e.g., 5/8 young). New loads enter the old
   region; re-access within a time window promotes to young. This protects
   hot catalog pages from being evicted by sequential scans.

3. **Long-term — Per-pool policy selection.** Allow each `BufferPool` to
   specify its eviction policy at creation (`LRU`, `CLOCK`, `MRU`, etc.).
   Scan-heavy temporary tablespaces can use MRU while OLTP data pools use
   CLOCK or midpoint LRU — matching DB2's configurable approach.

**Current assessment:** Strict LRU is correct and sufficient for the current
single-session, low-concurrency stage. The `VecDeque` implementation is easy
to reason about and test. Upgrading to CLOCK is the natural first step when
profiling shows `retain()` overhead or scan pollution becomes measurable.

### Free-Space Tracking: In-Memory `Vec<bool>` vs On-Disk Bitmaps

| | Current | Alternative (Oracle ASSM-style) |
|---|---|---|
| **Design** | In-memory boolean per page + planned current-page hint | On-disk bitmap blocks with graduated fullness levels (0–25%, 25–50%, etc.) |
| **Pro** | Trivial to implement and understand | Survives crash; scales to millions of pages; low insert contention |
| **Con** | Lost on crash (rebuilt optimistically); linear O(n) scan | Bitmap blocks consume space; L1/L2/L3 tree adds implementation cost |

**Potential upgrade (incremental):**
1. **Near-term:** Persist the free map as a header page (page 0) in each
   `.DAT` file — gives crash durability without full ASSM complexity.
2. **Medium-term:** Upgrade to a 2-bit-per-page encoding
   (empty / <50% / <75% / full) to reduce wasted probes on insert.
3. **Long-term:** Full bitmap tree (ASSM-style) when page counts reach
   hundreds of thousands.

### Delete Model: Tombstone vs In-Place Delete + Undo

| | Current | Alternative (Oracle-style) |
|---|---|---|
| **Design** | Tombstone slot `(offset=0, length=0)`; dead bytes remain until compaction | In-place delete with undo segment; space reclaimable immediately |
| **Pro** | Simple, testable; natural fit for MVCC (old versions stay in place) | Immediate space reuse within the same block by other transactions |
| **Con** | Dead space accumulates; needs future `REORG` / compaction pass | Requires undo segments, ITL entries, and concurrency control for block-level contention |

**Potential upgrade:** Implement an online page compaction (`REORG`) that
reclaims tombstoned space without blocking readers. This closes the space
gap without the full undo-segment machinery.

### Row Addressing: RID vs Self-Contained ROWID

| | Current | Alternative (Oracle-style) |
|---|---|---|
| **Design** | `RID(page_id, slot_index)` — two integers, resolved via tablespace manager | `ROWID(object_id, file#, block#, row#)` — self-contained physical address |
| **Pro** | Simple; slot directory enables in-page reorg without changing RIDs | Any layer can resolve the physical location without a catalog lookup |
| **Con** | Requires external lookup to find which `.DAT` file a RID belongs to | 10-byte encoding; tightly couples address to physical layout; row migration invalidates ROWIDs |

**Current assessment:** With one heap file per table, the simpler RID is
sufficient. A self-contained ROWID would only pay off with multiple
datafiles per tablespace, which is not currently in scope.

### Recovery: ARIES WAL vs Redo + Undo Split

| | Current (ARIES) | Alternative (Oracle-style) |
|---|---|---|
| **Design** | Single WAL handles both redo and undo | Separate redo log + undo tablespace |
| **Pro** | One log, one recovery algorithm; well-studied with clear correctness proofs | Undo segments provide read consistency for free; redo log can be smaller |
| **Con** | WAL grows larger under long-running transactions; read-consistent snapshots need a separate version store | Two subsystems to size and manage; `ORA-01555: snapshot too old` when undo undersized |

**Current assessment:** ARIES is the right foundation — it gets RustDB to
correct ACID transactions with minimal code. If high-concurrency OLTP demands
it later, a version store layered alongside the WAL can provide Oracle-style
read consistency without abandoning the single-log model.

### Summary

| Area | Current approach | Complexity | Performance ceiling |
|------|-----------------|------------|-------------------|
| Buffer pools | Named, per-tablespace | Low | Medium (manual tuning) |
| Free-space map | In-memory `Vec<bool>` | Trivial | Low (linear scan, lost on crash) |
| Deletes | Tombstone | Low | Medium (needs compaction) |
| Row addressing | `RID(page, slot)` | Low | Sufficient for single-file tables |
| Recovery | ARIES WAL | Medium | High (proven at scale) |

The DB2-style architecture prioritises **correctness, testability, and
simplicity** first. Each area above has a clear upgrade path when real
workload data reveals the bottleneck — no premature optimisation required.

## New Dependencies

| Crate       | Phase | Purpose          |
|-------------|-------|------------------|
| `crc32fast` | 1     | Page checksums   |
