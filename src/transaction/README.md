# transaction/

Write-ahead log (WAL), transaction management, and ARIES-style recovery.

## Design Decisions

- **Bootstrap is not WAL-logged.** Database creation (`bootstrap`) is a one-time
  operation with no prior consistent state to recover to. If bootstrap fails,
  the data directory is deleted and re-created. WAL logging begins only after
  the database is fully initialized and operational.

- **DB2-style in-place UPDATE.** Rows are overwritten directly in their
  existing page slot. Old row data (before-image) is preserved only in the
  WAL undo record — not on the data page. This keeps data pages compact
  and eliminates the need for a VACUUM process.

- **WAL-first contract.** Every data mutation writes its log record to the
  WAL **before** the data page is modified. On crash, the WAL is the source
  of truth for recovery.

## Architecture

```
SQL Executor
    │
    ├─ txn.begin()                    → allocate TxnId
    │
    ├─ per DML statement:
    │    ├─ WAL.write(log_record)     → append to WAL buffer
    │    ├─ WAL.flush()               → fsync log to disk
    │    └─ page mutation             → modify buffer pool page
    │
    ├─ txn.commit()                   → write COMMIT record, flush WAL
    └─ txn.rollback()                → apply undo records in reverse
```

## WAL Log Record Format

Each log record is a variable-length entry in the WAL file:

```
┌──────────────────────────────────────────────────────┐
│ Log Record Header (fixed 40 bytes)                   │
│   lsn            : u64     — Log Sequence Number     │
│   txn_id         : u64     — Transaction ID          │
│   record_type    : u8      — see table below         │
│   table_id       : u32     — (schema_hash, table_id) │
│   page_id        : u64     — affected page           │
│   slot           : u16     — affected slot            │
│   prev_lsn       : u64     — previous LSN for txn    │
│   body_len       : u32     — length of body bytes    │
├──────────────────────────────────────────────────────┤
│ Body (variable length)                               │
│   depends on record_type — see below                 │
└──────────────────────────────────────────────────────┘
```

All multi-byte values are **little-endian**, consistent with the page format.

### Record Types

| Type | Value | Body Contents | Undo Action | Redo Action |
|------|-------|---------------|-------------|-------------|
| `BEGIN` | 1 | (empty) | N/A | N/A |
| `COMMIT` | 2 | (empty) | N/A | N/A |
| `ROLLBACK` | 3 | (empty) | N/A | N/A |
| `INSERT` | 10 | `[new_row_bytes]` | Delete the inserted row | Re-insert the row |
| `DELETE` | 11 | `[old_row_bytes]` | Re-insert the deleted row | Delete the row again |
| `UPDATE` | 12 | `[old_len: u32][old_row_bytes][new_row_bytes]` | Overwrite with old_row | Overwrite with new_row |

### LSN (Log Sequence Number)

The LSN is a **monotonically increasing u64** that uniquely identifies each
log record's position. It serves as the ordering key for recovery.

Each data page also carries a `page_lsn` (stored in the page header's
reserved bytes 17–24) that records the LSN of the last log record applied
to that page. During recovery:
- **Redo:** Skip pages where `page_lsn >= log_record.lsn` (already applied).
- **Undo:** Only undo records for uncommitted transactions.

### Transaction Chain (prev_lsn)

Each log record contains a `prev_lsn` field pointing to the previous log
record for the same transaction. This forms a per-transaction chain that
enables efficient rollback without scanning the entire log:

```
TXN 42:  BEGIN ←── INSERT ←── UPDATE ←── DELETE ←── (current)
         lsn=10   lsn=15     lsn=23     lsn=31
         prev=0   prev=10    prev=15    prev=23
```

**Rollback** follows `prev_lsn` backwards, applying undo actions in reverse order.

## WAL File Layout

```
data/TESTDB/log/
├── WAL.000000     — first WAL segment (fixed size, e.g. 16 MB)
├── WAL.000001     — second segment
└── ...
```

Each segment is a sequence of packed log records. The WAL writer appends
to the current segment and rolls over to a new one when the segment is full.

## Write Path (WAL-First Protocol)

For each DML operation:

```
1. Allocate LSN (monotonic counter)
2. Build log record with before-image (undo) and after-image (redo)
3. Append record to WAL buffer
4. Flush WAL buffer to disk (fsync)          ← WAL is now durable
5. Apply mutation to the buffer pool page    ← page may be lost on crash
6. Set page_lsn = record.lsn
```

The key invariant: **a data page is never written to disk until all log
records that modified it have been flushed to the WAL.** This is enforced
at buffer pool eviction time — before evicting a dirty page, check that
`page_lsn ≤ flushed_lsn`.

## Rollback Protocol

When a transaction calls `rollback()`:

1. Read the transaction's last LSN from the active transaction table.
2. Follow the `prev_lsn` chain backwards.
3. For each record, apply the **undo action**:
   - INSERT → delete the row at (page_id, slot)
   - DELETE → re-insert the old row bytes at (page_id, slot)
   - UPDATE → overwrite with old_row_bytes at (page_id, slot)
4. Write a ROLLBACK record to the WAL.
5. Remove from active transaction table.

Each undo operation is itself logged as a **Compensation Log Record (CLR)**
so that undo work is not repeated if a crash occurs during rollback.

## ARIES Recovery (Crash Restart)

ARIES recovery runs in three phases after a crash:

```
Phase 1: Analysis
  └─ Scan WAL forward from last checkpoint
  └─ Rebuild active transaction table + dirty page table

Phase 2: Redo
  └─ Scan WAL forward, redo all operations
  └─ Skip records where page_lsn >= record.lsn (already applied)
  └─ Brings all pages to their most recent state

Phase 3: Undo
  └─ For each transaction in the active table (not committed):
       └─ Follow prev_lsn chain, apply undo actions
       └─ Write CLRs for each undo
  └─ All uncommitted work is rolled back
```

## Transaction API (Planned)

```rust
pub struct Transaction {
    txn_id: u64,
    state: TxnState,       // Active, Committed, RolledBack
    last_lsn: u64,         // tail of the prev_lsn chain
}

pub struct TransactionManager {
    wal: WalWriter,
    next_txn_id: u64,
    active_txns: HashMap<u64, Transaction>,
}

impl TransactionManager {
    pub fn begin(&mut self) -> u64;                    // returns txn_id
    pub fn commit(&mut self, txn_id: u64) -> Result<()>;
    pub fn rollback(&mut self, txn_id: u64) -> Result<()>;

    // Called by the executor before each page mutation:
    pub fn log_insert(&mut self, txn_id: u64, table_id: u32,
                      page_id: u64, slot: u16,
                      new_row: &[u8]) -> Result<u64>;  // returns LSN

    pub fn log_delete(&mut self, txn_id: u64, table_id: u32,
                      page_id: u64, slot: u16,
                      old_row: &[u8]) -> Result<u64>;

    pub fn log_update(&mut self, txn_id: u64, table_id: u32,
                      page_id: u64, slot: u16,
                      old_row: &[u8], new_row: &[u8]) -> Result<u64>;
}
```

## Integration with Executor

The executor's DML flow changes from:

```
// OLD: no WAL logging
tsm.delete_row(schema, table, rid)?;
tsm.insert_row(schema, table, &new_bytes)?;
```

To:

```
// NEW: WAL-first with in-place update
let lsn = txn_mgr.log_update(txn_id, table_id,
                               rid.page_id, rid.slot,
                               &old_bytes, &new_bytes)?;
let result = tsm.update_row(schema, table, rid, &new_bytes)?;
// If migrated, log a CLR for the old location and a new INSERT log
```

## Implementation Phases

### Phase 1: WAL Infrastructure
- [ ] `WalWriter` — append-only log file with fsync
- [ ] Log record serialization/deserialization
- [ ] LSN allocation (atomic counter)
- [ ] WAL segment management (rollover)

### Phase 2: Transaction Lifecycle
- [ ] `TransactionManager` with begin/commit/rollback
- [ ] Active transaction table
- [ ] `prev_lsn` chain maintenance
- [ ] Rollback via undo chain

### Phase 3: DML Logging
- [ ] Wire INSERT/DELETE/UPDATE log calls into executor
- [ ] Page LSN tracking (store in page header reserved bytes)
- [ ] Buffer pool eviction check: `page_lsn ≤ flushed_lsn`

### Phase 4: ARIES Recovery
- [ ] Analysis phase (rebuild active txn + dirty page tables)
- [ ] Redo phase (forward scan, skip already-applied)
- [ ] Undo phase (backward chain, CLR generation)
- [ ] Checkpoint records (reduce recovery scan range)
