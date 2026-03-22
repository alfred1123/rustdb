//! Transaction manager for MVCC.
//!
//! Provides TxID allocation (monotonic u64), transaction state tracking
//! (Active / Committed / Aborted), and the BEGIN / COMMIT / ROLLBACK lifecycle.
//!
//! TxID 0 is reserved for bootstrap rows and is always considered committed.

use std::collections::HashMap;

use crate::storage::tuple::TxId;

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
}

impl TxManager {
    /// Create a new transaction manager starting from the given next TxID.
    ///
    /// `next_id` should be loaded from persisted config (SQLDBCONF).
    /// Pass 1 for a fresh database (TxID 0 is reserved for bootstrap).
    pub fn new(next_id: TxId) -> Self {
        Self {
            next_id,
            states: HashMap::new(),
        }
    }

    /// Begin a new transaction. Returns the allocated TxID.
    pub fn begin(&mut self) -> TxId {
        let txid = self.next_id;
        self.next_id += 1;
        self.states.insert(txid, TxState::Active);
        txid
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
        let mut tm = TxManager::new(1);
        assert_eq!(tm.begin(), 1);
        assert_eq!(tm.begin(), 2);
        assert_eq!(tm.begin(), 3);
        assert_eq!(tm.next_id(), 4);
    }

    #[test]
    fn commit_active_tx() {
        let mut tm = TxManager::new(1);
        let tx = tm.begin();
        assert!(tm.is_active(tx));
        assert!(!tm.is_committed(tx));

        assert!(tm.commit(tx));
        assert!(tm.is_committed(tx));
        assert!(!tm.is_active(tx));
    }

    #[test]
    fn abort_active_tx() {
        let mut tm = TxManager::new(1);
        let tx = tm.begin();
        assert!(tm.abort(tx));
        assert!(!tm.is_committed(tx));
        assert!(!tm.is_active(tx));
        assert_eq!(tm.state(tx), Some(TxState::Aborted));
    }

    #[test]
    fn commit_non_active_returns_false() {
        let mut tm = TxManager::new(1);
        let tx = tm.begin();
        tm.abort(tx);
        assert!(!tm.commit(tx)); // already aborted
    }

    #[test]
    fn abort_non_active_returns_false() {
        let mut tm = TxManager::new(1);
        let tx = tm.begin();
        tm.commit(tx);
        assert!(!tm.abort(tx)); // already committed
    }

    #[test]
    fn unknown_txid_treated_as_not_committed() {
        let tm = TxManager::new(1);
        assert!(!tm.is_committed(42));
        assert!(!tm.is_active(42));
    }

    #[test]
    fn bootstrap_txid_always_committed() {
        let tm = TxManager::new(1);
        assert!(tm.is_committed(0));
        assert_eq!(tm.state(0), Some(TxState::Committed));
    }

    #[test]
    fn committed_checker_closure() {
        let mut tm = TxManager::new(1);
        let tx1 = tm.begin();
        let tx2 = tm.begin();
        tm.commit(tx1);

        let checker = tm.committed_checker();
        assert!(checker(0));   // bootstrap
        assert!(checker(tx1)); // committed
        assert!(!checker(tx2)); // still active
        assert!(!checker(99)); // unknown
    }

    #[test]
    fn double_commit_returns_false() {
        let mut tm = TxManager::new(1);
        let tx = tm.begin();
        assert!(tm.commit(tx));
        assert!(!tm.commit(tx)); // already committed
    }

    #[test]
    fn interleaved_transactions() {
        let mut tm = TxManager::new(1);
        let tx1 = tm.begin();
        let tx2 = tm.begin();
        let tx3 = tm.begin();

        tm.commit(tx1);
        tm.abort(tx3);

        assert!(tm.is_committed(tx1));
        assert!(tm.is_active(tx2));
        assert_eq!(tm.state(tx3), Some(TxState::Aborted));
    }
}
