# transaction/

MVCC transaction manager.

## Files (planned)

| File    | Purpose                                                                    |
|---------|----------------------------------------------------------------------------|
| `tx.rs` | Transaction manager: TxID allocation, state tracking, BEGIN/COMMIT/ROLLBACK |

Related modules in other directories:

- [`src/storage/tuple.rs`](../storage/tuple.rs) — Tuple header (xmin/xmax)
  serialization and visibility checks (on-disk row format, lives with
  `page.rs`, `heap.rs`, `tablespace.rs`)
- [`src/storage/page.rs`](../storage/page.rs) — `compact_page` for VACUUM
  (page-level compaction)

---

## Design: PostgreSQL-Style MVCC

RQDB is moving from physical delete / in-place update to a PostgreSQL-style
**multi-version concurrency control (MVCC)** model. The core trade-off:
sacrifice storage for performance — dead rows stay on the page until `VACUUM`
reclaims the space, avoiding costly in-place mutations and enabling future
concurrent-reader support.

### Tuple Header

Every row (user tables and system catalog) is prefixed with a 16-byte
visibility header:

```
Offset  Field   Size    Meaning
0       xmin    u64     TxID that created this tuple (0 = bootstrap)
8       xmax    u64     TxID that deleted/superseded it (0 = live)
```

The header lives inside the slotted-page row data. `page.rs` remains generic
(it stores opaque byte slices); tuple semantics are handled one layer up in
the tablespace manager and executor.

#### On-Disk Layout (Slotted Page with Tuple Headers)

```
Page (4096 bytes default)
┌──────────────────────────────────────────────────────────────┐
│ Page Header (checksum, slot_count, free_start, free_end)     │
├──────────────────────────────────────────────────────────────┤
│ Slot Directory (grows →)                                     │
│  slot 0: (offset=4050, len=46)                               │
│  slot 1: (offset=4000, len=50)                               │
│  slot 2: (offset=3960, len=40)  ← deleted row               │
│  slot 3: (offset=3910, len=50)  ← new version of slot 2     │
├──────────────────────────────────────────────────────────────┤
│                    Free Space                                │
├──────────────────────────────────────────────────────────────┤
│ Row Data (grows ←)                                           │
│                                                              │
│  slot 3: ┌─────────┬─────────┬──────────────────────┐       │
│          │ xmin=10  │ xmax=0  │ user columns...      │       │
│          └─────────┴─────────┴──────────────────────┘       │
│  slot 2: ┌─────────┬─────────┬──────────────────────┐       │
│          │ xmin=5   │ xmax=10 │ user columns (old)   │  DEAD │
│          └─────────┴─────────┴──────────────────────┘       │
│  slot 1: ┌─────────┬─────────┬──────────────────────┐       │
│          │ xmin=3   │ xmax=0  │ user columns...      │       │
│          └─────────┴─────────┴──────────────────────┘       │
│  slot 0: ┌─────────┬─────────┬──────────────────────┐       │
│          │ xmin=0   │ xmax=0  │ user columns...      │       │
│          └─────────┴─────────┴──────────────────────┘       │
└──────────────────────────────────────────────────────────────┘
         8 bytes   8 bytes     variable
         ◄─── 16-byte MVCC header ──►
```

#### xmin / xmax State Table

The combination of `xmin` and `xmax` values encodes the complete lifecycle
of a tuple:

```
┌───────────────────┬───────────┬───────────────────────────────────────────┐
│ xmin              │ xmax      │ Meaning                                   │
├───────────────────┼───────────┼───────────────────────────────────────────┤
│ 0 (bootstrap)     │ 0         │ Catalog row, always visible               │
│ committed TxID    │ 0         │ Live row — visible to all transactions    │
│ active TxID       │ 0         │ Uncommitted INSERT — visible only to the  │
│                   │           │   inserting transaction                   │
│ committed TxID    │ committed │ Dead row — deleted or superseded;         │
│                   │   TxID    │   invisible to all, reclaimable by VACUUM │
│ committed TxID    │ active    │ Row being deleted — still visible to      │
│                   │   TxID    │   other transactions (not yet committed)  │
│ committed TxID    │ aborted   │ Delete was rolled back — row is alive     │
│                   │   TxID    │   again (xmax is ignored)                 │
│ aborted TxID      │ (any)     │ Insert was rolled back — row never        │
│                   │           │   existed; invisible to all               │
└───────────────────┴───────────┴───────────────────────────────────────────┘
```

#### Worked Examples

**1. Simple INSERT + COMMIT**
```
BEGIN;          -- TxID = 5 allocated
INSERT ...;    -- row written: xmin=5, xmax=0
COMMIT;        -- TxID 5 → Committed

Result: row is visible to all future transactions
        (xmin=5 is committed, xmax=0 means live)
```

**2. INSERT + ROLLBACK**
```
BEGIN;          -- TxID = 6
INSERT ...;    -- row written: xmin=6, xmax=0
ROLLBACK;      -- TxID 6 → Aborted

Result: row is invisible to everyone
        (xmin=6 is aborted → row never existed)
        VACUUM will eventually reclaim the space
```

**3. DELETE a live row**
```
-- existing row: xmin=3 (committed), xmax=0

BEGIN;          -- TxID = 7
DELETE ...;    -- set xmax=7 on the row (in-place 8-byte write)
COMMIT;        -- TxID 7 → Committed

Result: row is dead (xmin=3 committed, xmax=7 committed)
        invisible to all future transactions
        VACUUM-reclaimable
```

**4. DELETE + ROLLBACK (row resurfaces)**
```
-- existing row: xmin=3 (committed), xmax=0

BEGIN;          -- TxID = 8
DELETE ...;    -- set xmax=8 on the row
ROLLBACK;      -- TxID 8 → Aborted

Result: row is alive again!
        (xmin=3 committed, xmax=8 aborted → xmax ignored)
```

**5. UPDATE (append-only: old row dies, new row born)**
```
-- existing row (slot 2): xmin=5 (committed), xmax=0

BEGIN;          -- TxID = 10
UPDATE ...;    -- step 1: set xmax=10 on slot 2
               -- step 2: insert new row (slot 3): xmin=10, xmax=0
COMMIT;        -- TxID 10 → Committed

Result: slot 2 is dead  (xmin=5 committed, xmax=10 committed)
        slot 3 is live  (xmin=10 committed, xmax=0)
```

### Visibility Rule

A tuple is **visible** to a transaction if:

- `xmin` is committed (or is the current transaction), AND
- `xmax == 0` OR `xmax` is aborted (or not yet committed)

A tuple is **dead** (reclaimable by VACUUM) when:

- `xmax` is committed

For the current single-user mode, "committed" simply means the transaction
completed successfully. Once concurrent sessions are added, a snapshot-based
visibility check (like PostgreSQL's `HeapTupleSatisfiesMVCC`) replaces this.

### Operation Changes

| Operation | Before (current)            | After (MVCC)                                                        |
|-----------|-----------------------------|---------------------------------------------------------------------|
| INSERT    | Write raw row bytes         | Prepend `(xmin=current_tx, xmax=0)` + row bytes                    |
| DELETE    | Zero slot `(0,0)`           | Set `xmax = current_tx` in the tuple header on-page                 |
| UPDATE    | In-place overwrite or migrate | Set `xmax` on old tuple + INSERT new tuple (append-only)          |
| SCAN      | Skip zeroed slots           | Read tuple header, apply visibility rule, return only visible rows  |
| VACUUM    | N/A                         | Compact pages: remove dead tuples, reclaim space, update FSM        |

### Why Append-Only Updates?

In-place updates require handling three cases (same size, larger-fits,
larger-doesn't-fit) and introduce complexity around row migration, RID
stability, and free-space tracking. Append-only updates (mark old dead +
insert new) simplify this to a single code path, avoid page-level locking
contention for future concurrency, and align naturally with MVCC — the old
version stays readable by any transaction that started before the update.

---

## Transaction Manager

### TxID Allocation

- Monotonic `u64` counter, starting from 1
- TxID 0 is reserved for bootstrap rows (always considered committed)
- `next_txid` is persisted in `admin/TXLOG` (8-byte little-endian file,
  fsync'd on every `BEGIN`) so IDs never repeat across restarts
- **Future (WAL):** once WAL is implemented, the TxID is embedded in every
  WAL record and recovery simply takes `max(txid) + 1` from the log scan,
  making the TXLOG file redundant

### Transaction States

```
Active  ──→  Committed
  │
  └──→  Aborted
```

State is tracked in-memory: `HashMap<TxID, TxState>` where
`TxState = Active | Committed | Aborted`.

On startup, any TxID not in the committed set is treated as aborted
(crash recovery without WAL — safe for single-user mode).

### SQL Commands

| Command    | Behavior                                                                 |
|------------|--------------------------------------------------------------------------|
| `BEGIN`    | Allocate next TxID, mark `Active`. Error if a transaction is already open. |
| `COMMIT`   | Mark current TxID `Committed`, flush dirty pages.                       |
| `ROLLBACK` | Mark current TxID `Aborted`. Inserted tuples become invisible; deleted tuples' xmax is aborted so they reappear. |

Without an explicit `BEGIN`, each SQL statement runs in an implicit
auto-commit transaction (allocate TxID → execute → commit).

---

## MVCC Flow: How Visibility and Rollback Work

### INSERT Flow

```
User: INSERT INTO t VALUES (...)
          │
          ▼
┌─────────────────────────┐
│ Executor::execute_insert│
│  current_tx = 5         │
└─────────┬───────────────┘
          │
          ▼
┌─────────────────────────────────────────┐
│ tuple::prepend_header(xmin=5, row_data) │
│  → [xmin=5 | xmax=0 | col1 | col2 ...] │
└─────────┬───────────────────────────────┘
          │
          ▼
┌─────────────────────────────────────┐
│ TablespaceManager::insert_row       │
│  page.rs writes the full tuple      │
│  (header + user data) into a slot   │
│  FSM updated                        │
└─────────────────────────────────────┘
```

### DELETE Flow

```
User: DELETE FROM t WHERE id = 42
          │
          ▼
┌────────────────────────────────┐
│ Executor::execute_delete       │
│  current_tx = 7                │
│  scan all pages for matching   │
│  rows (visibility-checked)     │
└─────────┬──────────────────────┘
          │  for each matching row
          ▼
┌────────────────────────────────────────────┐
│ tuple::set_xmax_on_page(page_buf, slot, 7) │
│  writes xmax=7 into the 8 bytes at         │
│  row_offset+8 (in-place, no row copy)      │
└────────────────────────────────────────────┘
```

### UPDATE Flow (Append-Only)

```
User: UPDATE t SET col = 'new' WHERE id = 42
          │
          ▼
┌──────────────────────────────────────────┐
│ Executor::execute_update                  │
│  current_tx = 10                          │
│  scan for matching rows                   │
└─────────┬────────────────────────────────┘
          │  for each matching row
          ▼
┌──────────────────────────────────────────┐
│ Step 1: Mark old row dead                 │
│  set_xmax_on_page(old_page, old_slot, 10) │
└─────────┬────────────────────────────────┘
          │
          ▼
┌──────────────────────────────────────────┐
│ Step 2: Insert new version                │
│  prepend_header(xmin=10, new_row_data)    │
│  insert_row → new slot (possibly new page)│
└──────────────────────────────────────────┘
```

### SCAN (Visibility Check) Flow

```
User: SELECT * FROM t
          │
          ▼
┌──────────────────────────────────────────┐
│ Executor / TablespaceManager::scan        │
│  for each page:                           │
│    for each slot:                         │
│      read_header → (xmin, xmax)           │
│              │                            │
│              ▼                            │
│      ┌──────────────────────────────┐     │
│      │ tuple::is_visible(           │     │
│      │   xmin, xmax,               │     │
│      │   current_tx,               │     │
│      │   tx_mgr.committed_checker()│     │
│      │ )                           │     │
│      └──────┬───────────────────────┘     │
│             │                             │
│        ┌────┴────┐                        │
│        │ visible │                        │
│      ┌─┴─┐    ┌──┴──┐                    │
│      │YES│    │ NO  │                     │
│      └─┬─┘    └──┬──┘                    │
│        │         │                        │
│   return row   skip                       │
└──────────────────────────────────────────┘
```

### ROLLBACK Flow (How Dead Rows Resurface)

ROLLBACK does **not** undo any physical writes. Instead, it marks the
TxID as `Aborted` in the transaction manager. All visibility decisions
then automatically ignore that TxID's effects:

```
User: ROLLBACK
          │
          ▼
┌────────────────────────────────────────────────────┐
│ TxManager::abort(current_tx = 8)                    │
│  states[8] = Aborted                                │
│                                                     │
│  (No pages are modified. No rows are rewritten.     │
│   The abort is purely a state-table update.)        │
└────────────────────────────────────────────────────┘

What happens on subsequent scans:

  Row A: xmin=8, xmax=0   (INSERTed by aborted Tx 8)
    → is_visible: xmin=8 is aborted → NOT visible
    → The row is treated as if it was never inserted
    → VACUUM can reclaim the space later

  Row B: xmin=3(committed), xmax=8   (DELETEd by aborted Tx 8)
    → is_visible: xmin=3 is committed ✓
                  xmax=8 is aborted → xmax ignored ✓
    → The row is VISIBLE again — delete was undone!

  Row C: xmin=3(committed), xmax=8   (old version from UPDATE by Tx 8)
    → Same as Row B: old version reappears

  Row D: xmin=8, xmax=0   (new version from UPDATE by Tx 8)
    → Same as Row A: new version vanishes
```

### COMMIT Flow

```
User: COMMIT
          │
          ▼
┌──────────────────────────────────────────┐
│ TxManager::commit(current_tx = 5)         │
│  states[5] = Committed                    │
└─────────┬────────────────────────────────┘
          │
          ▼
┌──────────────────────────────────────────┐
│ BufferPool::flush_all()                   │
│  write all dirty pages to disk            │
│  (ensures durability without WAL)         │
└──────────────────────────────────────────┘

All tuples with xmin=5 are now permanently visible.
All tuples with xmax=5 are now permanently dead.
```

### End-to-End: Transaction Lifecycle on the State Table

```
Time  Action         TxManager State       Row Effects
─────────────────────────────────────────────────────────────
t1    BEGIN           states[5] = Active    —
t2    INSERT row A    (no state change)     A: xmin=5, xmax=0
t3    DELETE row B    (no state change)     B: xmax=5
t4a   COMMIT          states[5] = Committed A visible, B dead
      ─── OR ───
t4b   ROLLBACK        states[5] = Aborted   A invisible, B alive
```

---

## Two-Page UPDATE Problem

With append-only updates, an UPDATE touches **two pages**: exclusive latch
on page A (set xmax on old row), then exclusive latch on page B (insert new
row). This has performance and concurrency implications.

### Industry Solutions

| Strategy | Used by | How it works | Trade-off |
|----------|---------|--------------|-----------|
| **PCTFREE / fillfactor** | DB2, Oracle, PostgreSQL | Reserve page space so the new version often fits on the same page | Wastes space on read-heavy tables |
| **HOT (Heap-Only Tuples)** | PostgreSQL | If new version fits on same page AND no indexed columns changed, chain old→new without index update | Only works when no indexed columns change; requires indexes |
| **Undo-based MVCC** | MySQL/InnoDB, Oracle | UPDATE is always in-place; old versions reconstructed from undo log | Undo log management complexity; long transactions bloat undo |
| **Forwarding pointers** | Oracle (row migration) | Leave a stub on the original page pointing to the new location | Extra I/O for reads that follow the pointer |
| **Delta storage** | Column stores, HTAP systems | Store only changed columns as a delta, not a full new row | Reconstruction cost on read; complex merge logic |
| **In-place for same-size** | DB2 | Keep in-place UPDATE when row size doesn't change | Two code paths; doesn't help when row grows |

**RQDB's approach:** PCTFREE (implemented per-table in SYSTABLES — see
[storage/README.md](../storage/README.md)) is the primary solution. HOT is a
natural follow-on once B-tree indexes are added. Undo-based MVCC is a
fundamentally different architecture and not on the roadmap.

### Multi-Session Deadlock (Future)

When concurrent sessions are added, two transactions updating each other's
pages could deadlock:

```
Tx1: latch page 5 → wants page 8
Tx2: latch page 8 → wants page 5
```

**Mitigation strategies** (for the future multi-session milestone):

1. **Latch ordering**: Always acquire page latches in ascending `page_id`
   order.
2. **Release-before-acquire**: Release the latch on the old-row page after
   writing xmax, before acquiring the latch on the new-row page. Safe
   because xmax is already durable in the frame.
3. **Latch timeout + retry**: Abort and retry if a latch cannot be acquired
   within a deadline.

None of these require changes now — single-user mode has no contention.

---

## Dirty Pages and Latches — Compatibility with MVCC

The current buffer pool model (`pool.rs`) is **fully compatible with
single-user MVCC**:

| Concern | Assessment |
|---------|------------|
| DELETE (set xmax, 8 bytes) | `fetch_page_mut` → write xmax → `unpin(dirty=true)`. Same pattern as today's slot-zeroing. |
| INSERT (prepend header + row) | Same as today, just more bytes per row (16-byte header overhead). |
| UPDATE (xmax on old + insert new) | Two sequential exclusive latches: page A then page B. No issue in single-user mode. |
| SCAN (read tuple header) | `fetch_page` → read header → check visibility → `unpin`. Same shared-latch pattern. |
| Dirty page volume | UPDATE dirties 2 pages instead of 1. LRU + `flush_all` handles this transparently. |
| VACUUM (compact page) | `fetch_page_mut` → rebuild page → `unpin(dirty=true)`. Standard exclusive-latch write. |

PCTFREE reduces the two-page UPDATE to a single-page operation when the
new version fits in the reserved space — one exclusive latch, one dirty page.

---

## File-by-File Changes

### New files

- **`src/transaction/tx.rs`** — Transaction manager
  - `struct TxManager { next_id: u64, states: HashMap<TxID, TxState> }`
  - `begin() -> TxID`, `commit(TxID)`, `abort(TxID)`, `is_committed(TxID) -> bool`
  - Persistence: `next_id` saved to `admin/TXLOG` (8-byte LE file, fsync'd
    on every BEGIN). Future: WAL replaces TXLOG.

- **`src/storage/tuple.rs`** — Tuple header helpers (in `storage/`)
  - `const TUPLE_HEADER_SIZE: usize = 16`
  - `write_header(xmin, xmax) -> [u8; 16]`
  - `read_header(bytes) -> (xmin, xmax)`
  - `set_xmax(buf, slot, xmax)` — mutate xmax in-place on a page
  - `strip_header(bytes) -> &[u8]` — return user data portion
  - `prepend_header(xmin, row_data) -> Vec<u8>`

### Modified files

- **`src/storage/page.rs`** — Add `compact_page(buf)` for VACUUM
- **`src/storage/tablespace.rs`** — INSERT prepends header; DELETE sets xmax;
  UPDATE becomes append-only; SCAN applies visibility; respects PCTFREE
- **`src/sql/executor.rs`** — Pass TxID to DML; dispatch BEGIN/COMMIT/ROLLBACK;
  add VACUUM command
- **`src/catalog/bootstrap.rs`** — Bootstrap rows get `xmin = 0`
- **`src/catalog/loader.rs`** — Strip tuple headers when loading catalog
- **`src/catalog/config.rs`** — `FORMAT_VERSION` for backward-compat detection
- **`src/db.rs`** — Add `TxManager` to Database struct
- **`src/main.rs`** — Parse BEGIN/COMMIT/ROLLBACK in REPL
- **`src/error.rs`** — Add `ActiveTransaction`, `NoActiveTransaction` SQLSTATE codes

---

## Implementation Order

1. Tuple header module (`src/storage/tuple.rs`)
2. Transaction manager (`src/transaction/tx.rs`)
3. Bootstrap + config changes (catalog rows get headers; TXLOG stores next_txid)
4. Tablespace changes (insert/delete/update/scan with tuple headers + visibility)
5. PCTFREE in SYSTABLES + FSM-aware insert (see [storage/README.md](../storage/README.md))
6. Executor changes (pass TxID, wire BEGIN/COMMIT/ROLLBACK, auto-commit)
7. VACUUM implementation (page compaction + SQL command — see [storage/README.md](../storage/README.md))
8. REPL integration (BEGIN/COMMIT/ROLLBACK in the shell)
9. Tests (update existing, add new for transactions/visibility/rollback/VACUUM)

## Migration

This is a **breaking on-disk format change**. A format version field in
SQLDBCONF rejects old-format databases with a clear error message.
