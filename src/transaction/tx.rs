//! Transaction manager for MVCC.
//!
//! Provides TxID allocation (monotonic u64), transaction state tracking
//! (Active / Committed / Aborted), and the BEGIN / COMMIT / ROLLBACK lifecycle.
//!
//! TxID 0 is reserved for bootstrap rows and is always considered committed.
//!
//! ## TxID Persistence
//!
//! `next_txid` is stored in a dedicated `admin/TXLOG` file (8 bytes, little-
//! endian u64). This file is fsync'd on every `BEGIN` so that a crash can
//! never cause TxID reuse. When WAL is added in the future, the TxID will
//! be embedded in each WAL record and `TXLOG` becomes redundant — the
//! recovery scan simply takes `max(txid) + 1` from the WAL.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::storage::tuple::TxId;

const TXLOG_FILENAME: &str = "TXLOG";

/// Transaction state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxState {
    Active,
    Committed,
    Aborted,
}

/// Transaction manager.
///
/// Owns the monotonic TxID counter and the in-memory state table.
/// On startup, any TxID not explicitly in the state table is treated
/// as aborted (safe for single-user mode without WAL).
#[derive(Debug)]
pub struct TxManager {
    next_id: TxId,
    states: HashMap<TxId, TxState>,
    /// Path to admin/TXLOG. `None` when running in-memory (unit tests).
    txlog_path: Option<PathBuf>,
}

impl TxManager {
    /// Create a new transaction manager backed by a persistent TXLOG file.
    ///
    /// Reads `next_id` from `<db_dir>/admin/TXLOG` if it exists; otherwise
    /// starts from 1 (TxID 0 is reserved for bootstrap).
    pub fn open(db_dir: &Path) -> Result<Self> {
        let txlog_path = db_dir.join("admin").join(TXLOG_FILENAME);
        let next_id = if txlog_path.exists() {
            let bytes = fs::read(&txlog_path).map_err(|e| {
                Error::Io(std::io::Error::new(e.kind(), format!("read TXLOG: {e}")))
            })?;
            if bytes.len() < 8 {
                return Err(Error::Catalog("TXLOG is corrupted (too short)".into()));
            }
            u64::from_le_bytes(bytes[..8].try_into().unwrap())
        } else {
            1
        };
        Ok(Self {
            next_id,
            states: HashMap::new(),
            txlog_path: Some(txlog_path),
        })
    }

    /// Create an in-memory-only transaction manager (for unit tests).
    pub fn new_in_memory(next_id: TxId) -> Self {
        Self {
            next_id,
            states: HashMap::new(),
            txlog_path: None,
        }
    }

    /// Persist the current `next_id` to the TXLOG file (fsync'd).
    fn persist_next_id(&self) -> Result<()> {
        if let Some(ref path) = self.txlog_path {
            let mut f = fs::File::create(path).map_err(|e| {
                Error::Io(std::io::Error::new(e.kind(), format!("write TXLOG: {e}")))
            })?;
            f.write_all(&self.next_id.to_le_bytes()).map_err(|e| {
                Error::Io(std::io::Error::new(e.kind(), format!("write TXLOG: {e}")))
            })?;
            f.sync_all().map_err(|e| {
                Error::Io(std::io::Error::new(e.kind(), format!("fsync TXLOG: {e}")))
            })?;
        }
        Ok(())
    }

    /// Begin a new transaction. Returns the allocated TxID.
    ///
    /// Persists the incremented `next_id` to TXLOG before returning, so
    /// the TxID is durable even if the process crashes before COMMIT.
    pub fn begin(&mut self) -> Result<TxId> {
        let txid = self.next_id;
        self.next_id += 1;
        self.persist_next_id()?;
        self.states.insert(txid, TxState::Active);
        Ok(txid)
    }

    /// Commit a transaction.
    ///
    /// Returns `true` if the transaction was active and is now committed.
    /// Returns `false` if the TxID was not active (already committed/aborted
    /// or unknown).
    pub fn commit(&mut self, txid: TxId) -> bool {
        match self.states.get(&txid) {
            Some(TxState::Active) => {
                self.states.insert(txid, TxState::Committed);
                true
            }
            _ => false,
        }
    }

    /// Abort (rollback) a transaction.
    ///
    /// Returns `true` if the transaction was active and is now aborted.
    pub fn abort(&mut self, txid: TxId) -> bool {
        match self.states.get(&txid) {
            Some(TxState::Active) => {
                self.states.insert(txid, TxState::Aborted);
                true
            }
            _ => false,
        }
    }

    /// Check whether a TxID is committed.
    ///
    /// TxID 0 (bootstrap) is always committed.
    /// Unknown TxIDs (not in the state table) are treated as aborted.
    pub fn is_committed(&self, txid: TxId) -> bool {
        if txid == 0 {
            return true;
        }
        self.states.get(&txid) == Some(&TxState::Committed)
    }

    /// Check whether a TxID is active.
    pub fn is_active(&self, txid: TxId) -> bool {
        self.states.get(&txid) == Some(&TxState::Active)
    }

    /// Get the state of a TxID.
    pub fn state(&self, txid: TxId) -> Option<TxState> {
        if txid == 0 {
            return Some(TxState::Committed);
        }
        self.states.get(&txid).copied()
    }

    /// The next TxID that will be allocated. Used for persistence.
    pub fn next_id(&self) -> TxId {
        self.next_id
    }

    /// Return a closure suitable for passing to `tuple::is_visible`.
    pub fn committed_checker(&self) -> impl Fn(TxId) -> bool + '_ {
        move |txid| self.is_committed(txid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_allocates_sequential_ids() {
        let mut tm = TxManager::new_in_memory(1);
        assert_eq!(tm.begin().unwrap(), 1);
        assert_eq!(tm.begin().unwrap(), 2);
        assert_eq!(tm.begin().unwrap(), 3);
        assert_eq!(tm.next_id(), 4);
    }

    #[test]
    fn commit_active_tx() {
        let mut tm = TxManager::new_in_memory(1);
        let tx = tm.begin().unwrap();
        assert!(tm.is_active(tx));
        assert!(!tm.is_committed(tx));

        assert!(tm.commit(tx));
        assert!(tm.is_committed(tx));
        assert!(!tm.is_active(tx));
    }

    #[test]
    fn abort_active_tx() {
        let mut tm = TxManager::new_in_memory(1);
        let tx = tm.begin().unwrap();
        assert!(tm.abort(tx));
        assert!(!tm.is_committed(tx));
        assert!(!tm.is_active(tx));
        assert_eq!(tm.state(tx), Some(TxState::Aborted));
    }

    #[test]
    fn commit_non_active_returns_false() {
        let mut tm = TxManager::new_in_memory(1);
        let tx = tm.begin().unwrap();
        tm.abort(tx);
        assert!(!tm.commit(tx));
    }

    #[test]
    fn abort_non_active_returns_false() {
        let mut tm = TxManager::new_in_memory(1);
        let tx = tm.begin().unwrap();
        tm.commit(tx);
        assert!(!tm.abort(tx));
    }

    #[test]
    fn unknown_txid_treated_as_not_committed() {
        let tm = TxManager::new_in_memory(1);
        assert!(!tm.is_committed(42));
        assert!(!tm.is_active(42));
    }

    #[test]
    fn bootstrap_txid_always_committed() {
        let tm = TxManager::new_in_memory(1);
        assert!(tm.is_committed(0));
        assert_eq!(tm.state(0), Some(TxState::Committed));
    }

    #[test]
    fn committed_checker_closure() {
        let mut tm = TxManager::new_in_memory(1);
        let tx1 = tm.begin().unwrap();
        let tx2 = tm.begin().unwrap();
        tm.commit(tx1);

        let checker = tm.committed_checker();
        assert!(checker(0));
        assert!(checker(tx1));
        assert!(!checker(tx2));
        assert!(!checker(99));
    }

    #[test]
    fn double_commit_returns_false() {
        let mut tm = TxManager::new_in_memory(1);
        let tx = tm.begin().unwrap();
        assert!(tm.commit(tx));
        assert!(!tm.commit(tx));
    }

    #[test]
    fn interleaved_transactions() {
        let mut tm = TxManager::new_in_memory(1);
        let tx1 = tm.begin().unwrap();
        let tx2 = tm.begin().unwrap();
        let tx3 = tm.begin().unwrap();

        tm.commit(tx1);
        tm.abort(tx3);

        assert!(tm.is_committed(tx1));
        assert!(tm.is_active(tx2));
        assert_eq!(tm.state(tx3), Some(TxState::Aborted));
    }

    #[test]
    fn txlog_persistence() {
        let dir = std::env::temp_dir().join("rqdb_test_txlog");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("admin")).unwrap();

        {
            let mut tm = TxManager::open(&dir).unwrap();
            assert_eq!(tm.next_id(), 1);
            let tx1 = tm.begin().unwrap();
            assert_eq!(tx1, 1);
            let tx2 = tm.begin().unwrap();
            assert_eq!(tx2, 2);
        }

        {
            let tm = TxManager::open(&dir).unwrap();
            assert_eq!(tm.next_id(), 3);
        }

        fs::remove_dir_all(&dir).unwrap();
    }
}
