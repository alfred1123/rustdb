//! Tuple header for MVCC visibility.
//!
//! Every row stored in a slotted page is prefixed with a 16-byte header:
//!
//! ```text
//! [xmin: u64 LE][xmax: u64 LE][user data ...]
//! ```
//!
//! - `xmin` — TxID that created this tuple (0 = bootstrap, always visible).
//! - `xmax` — TxID that deleted or superseded this tuple (0 = live).

/// Size of the per-tuple MVCC header in bytes.
pub const TUPLE_HEADER_SIZE: usize = 16;

/// Transaction ID type.
pub type TxId = u64;

/// TxID 0 is reserved for bootstrap rows and is always considered committed.
pub const BOOTSTRAP_TXID: TxId = 0;

/// Write a tuple header into a fixed-size array.
pub fn write_header(xmin: TxId, xmax: TxId) -> [u8; TUPLE_HEADER_SIZE] {
    let mut hdr = [0u8; TUPLE_HEADER_SIZE];
    hdr[0..8].copy_from_slice(&xmin.to_le_bytes());
    hdr[8..16].copy_from_slice(&xmax.to_le_bytes());
    hdr
}

/// Read xmin and xmax from the first 16 bytes of a tuple.
///
/// Returns `None` if `data` is shorter than `TUPLE_HEADER_SIZE`.
pub fn read_header(data: &[u8]) -> Option<(TxId, TxId)> {
    if data.len() < TUPLE_HEADER_SIZE {
        return None;
    }
    let xmin = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let xmax = u64::from_le_bytes(data[8..16].try_into().unwrap());
    Some((xmin, xmax))
}

/// Return the user-data portion of a tuple (everything after the header).
///
/// Returns `None` if `data` is shorter than `TUPLE_HEADER_SIZE`.
pub fn strip_header(data: &[u8]) -> Option<&[u8]> {
    if data.len() < TUPLE_HEADER_SIZE {
        return None;
    }
    Some(&data[TUPLE_HEADER_SIZE..])
}

/// Build a complete tuple: header + user data.
pub fn prepend_header(xmin: TxId, row_data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(TUPLE_HEADER_SIZE + row_data.len());
    buf.extend_from_slice(&write_header(xmin, 0));
    buf.extend_from_slice(row_data);
    buf
}

/// Mutate the xmax field of a tuple stored at `slot` within a raw page
/// buffer. This performs an in-place 8-byte write without moving the row.
///
/// Returns `false` if the slot is out of range or already deleted (offset=0).
pub fn set_xmax_on_page(
    page_buf: &mut [u8],
    slot: u16,
    xmax: TxId,
) -> bool {
    let slot_count = crate::storage::page::slot_count_of(page_buf);
    if slot >= slot_count {
        return false;
    }
    let (off, len) = crate::storage::page::slot_of(page_buf, slot);
    if off == 0 && len == 0 {
        return false;
    }
    if (len as usize) < TUPLE_HEADER_SIZE {
        return false;
    }
    let start = off as usize + 8; // xmax is at offset 8 within the tuple
    page_buf[start..start + 8].copy_from_slice(&xmax.to_le_bytes());
    crate::storage::page::update_checksum_of(page_buf);
    true
}

/// Check whether a tuple is visible to a given transaction.
///
/// `is_committed` is a callback that returns `true` if the given TxID has
/// been committed. TxID 0 (bootstrap) is always committed.
pub fn is_visible(
    xmin: TxId,
    xmax: TxId,
    current_tx: TxId,
    is_committed: &dyn Fn(TxId) -> bool,
) -> bool {
    let xmin_visible = xmin == BOOTSTRAP_TXID
        || xmin == current_tx
        || is_committed(xmin);

    if !xmin_visible {
        return false;
    }

    if xmax == 0 {
        return true;
    }

    // xmax is set — tuple was deleted/updated.
    // Visible only if the deleting tx is NOT committed (i.e., it was
    // aborted or is still in-flight for another transaction).
    if xmax == current_tx {
        return false; // our own delete — we shouldn't see it
    }

    !is_committed(xmax)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::page::{SlottedPage, PageRead, PageWrite};

    #[test]
    fn write_and_read_header() {
        let hdr = write_header(42, 0);
        let (xmin, xmax) = read_header(&hdr).unwrap();
        assert_eq!(xmin, 42);
        assert_eq!(xmax, 0);
    }

    #[test]
    fn write_and_read_nonzero_xmax() {
        let hdr = write_header(10, 25);
        let (xmin, xmax) = read_header(&hdr).unwrap();
        assert_eq!(xmin, 10);
        assert_eq!(xmax, 25);
    }

    #[test]
    fn read_header_too_short() {
        assert!(read_header(&[0u8; 15]).is_none());
        assert!(read_header(&[]).is_none());
    }

    #[test]
    fn strip_header_returns_user_data() {
        let user_data = b"hello world";
        let tuple = prepend_header(1, user_data);
        assert_eq!(tuple.len(), TUPLE_HEADER_SIZE + user_data.len());

        let stripped = strip_header(&tuple).unwrap();
        assert_eq!(stripped, user_data);
    }

    #[test]
    fn strip_header_too_short() {
        assert!(strip_header(&[0u8; 10]).is_none());
    }

    #[test]
    fn prepend_header_sets_xmin_and_zero_xmax() {
        let tuple = prepend_header(99, b"data");
        let (xmin, xmax) = read_header(&tuple).unwrap();
        assert_eq!(xmin, 99);
        assert_eq!(xmax, 0);
    }

    #[test]
    fn visibility_bootstrap_row() {
        let committed = |_: TxId| false;
        // xmin=0 (bootstrap) is always visible when xmax=0.
        assert!(is_visible(0, 0, 5, &committed));
    }

    #[test]
    fn visibility_own_insert() {
        let committed = |_: TxId| false;
        // Our own insert (xmin == current_tx) is visible.
        assert!(is_visible(5, 0, 5, &committed));
    }

    #[test]
    fn visibility_committed_insert() {
        let committed = |tx: TxId| tx == 3;
        // Committed insert, no delete.
        assert!(is_visible(3, 0, 5, &committed));
    }

    #[test]
    fn visibility_uncommitted_insert() {
        let committed = |_: TxId| false;
        // Another transaction's uncommitted insert — not visible.
        assert!(!is_visible(7, 0, 5, &committed));
    }

    #[test]
    fn visibility_deleted_by_committed_tx() {
        let committed = |tx: TxId| tx == 3 || tx == 4;
        // Created by tx 3 (committed), deleted by tx 4 (committed) — dead.
        assert!(!is_visible(3, 4, 5, &committed));
    }

    #[test]
    fn visibility_deleted_by_aborted_tx() {
        let committed = |tx: TxId| tx == 3;
        // Created by tx 3 (committed), deleted by tx 6 (aborted) — still visible.
        assert!(is_visible(3, 6, 5, &committed));
    }

    #[test]
    fn visibility_deleted_by_self() {
        let committed = |tx: TxId| tx == 3;
        // Created by tx 3, deleted by our own tx 5 — not visible to us.
        assert!(!is_visible(3, 5, 5, &committed));
    }

    #[test]
    fn visibility_deleted_by_inflight_other() {
        let committed = |tx: TxId| tx == 3;
        // Created by tx 3, xmax=7 which is in-flight (not committed) — visible.
        assert!(is_visible(3, 7, 5, &committed));
    }

    #[test]
    fn set_xmax_on_page_roundtrip() {
        let mut page = SlottedPage::new(0, 4096);
        let tuple = prepend_header(42, b"hello");
        let slot = page.insert_row(&tuple).unwrap();

        // xmax should be 0 initially.
        let raw = page.read_row(slot).unwrap();
        let (xmin, xmax) = read_header(raw).unwrap();
        assert_eq!(xmin, 42);
        assert_eq!(xmax, 0);

        // Set xmax to 99.
        assert!(set_xmax_on_page(page.buf_mut(), slot, 99));

        let raw = page.read_row(slot).unwrap();
        let (xmin, xmax) = read_header(raw).unwrap();
        assert_eq!(xmin, 42);
        assert_eq!(xmax, 99);
        assert_eq!(strip_header(raw).unwrap(), b"hello");
    }

    #[test]
    fn set_xmax_on_page_out_of_range() {
        let mut page = SlottedPage::new(0, 4096);
        assert!(!set_xmax_on_page(page.buf_mut(), 0, 1));
    }
}
