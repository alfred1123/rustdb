# transaction/

WAL, concurrency control, and recovery. Not yet implemented.

## Design Decisions

- **Bootstrap is not WAL-logged.** Database creation (`bootstrap`) is a one-time
  operation with no prior consistent state to recover to. If bootstrap fails,
  the data directory is deleted and re-created. WAL logging begins only after
  the database is fully initialized and operational.

## Planned

- Write-ahead log (WAL) — every mutation writes to the log before the data page
- ARIES-style crash recovery (redo/undo)
- Lock-based or MVCC concurrency control
