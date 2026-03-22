use crate::error::{Error, Result};

pub type PageId = u64;
pub type SlotIndex = u16;

// ── Page header layout (24 bytes) ──

const PAGE_ID_OFF: usize = 0;
const PAGE_TYPE_OFF: usize = 8;
const FREE_SPACE_OFF: usize = 9;
const SLOT_COUNT_OFF: usize = 11;
const CHECKSUM_OFF: usize = 13;
// reserved: 17..24
pub const PAGE_HEADER_SIZE: usize = 24;

const SLOT_SIZE: usize = 4; // u16 offset + u16 length

/// Page type flags.
pub const PAGE_TYPE_FREE: u8 = 0;
pub const PAGE_TYPE_DATA: u8 = 1;

// ── Free functions: shared page logic on raw byte slices ──
// These are the single source of truth — SlottedPage, PageRef, and PageMut
// all delegate to these so that data‐layout knowledge lives in one place.

pub(crate) fn page_id_of(buf: &[u8]) -> PageId {
    get_u64(buf, PAGE_ID_OFF)
}

fn page_type_of(buf: &[u8]) -> u8 {
    buf[PAGE_TYPE_OFF]
}

pub(crate) fn slot_count_of(buf: &[u8]) -> u16 {
    get_u16(buf, SLOT_COUNT_OFF)
}

pub(crate) fn free_space_of(buf: &[u8]) -> usize {
    let dir_end = PAGE_HEADER_SIZE + (slot_count_of(buf) as usize) * SLOT_SIZE;
    let data_start = get_u16(buf, FREE_SPACE_OFF) as usize;
    if data_start <= dir_end + SLOT_SIZE {
        0
    } else {
        data_start - dir_end - SLOT_SIZE
    }
}

pub(crate) fn slot_of(buf: &[u8], idx: SlotIndex) -> (u16, u16) {
    let base = PAGE_HEADER_SIZE + (idx as usize) * SLOT_SIZE;
    (get_u16(buf, base), get_u16(buf, base + 2))
}

fn read_row_from(buf: &[u8], slot: SlotIndex) -> Option<&[u8]> {
    if slot >= slot_count_of(buf) {
        return None;
    }
    let (off, len) = slot_of(buf, slot);
    if off == 0 && len == 0 {
        return None; // deleted
    }
    Some(&buf[off as usize..(off + len) as usize])
}

fn insert_row_into(buf: &mut [u8], row: &[u8]) -> Option<SlotIndex> {
    if free_space_of(buf) < row.len() {
        return None;
    }

    // Allocate row space (grows from end toward header).
    let data_start = get_u16(buf, FREE_SPACE_OFF) as usize;
    let new_data_start = data_start - row.len();
    buf[new_data_start..new_data_start + row.len()].copy_from_slice(row);
    put_u16(buf, FREE_SPACE_OFF, new_data_start as u16);

    // Check for a reusable deleted slot.
    let slot_count = slot_count_of(buf) as usize;
    let mut slot_idx = slot_count; // default: append new slot
    for i in 0..slot_count {
        let (off, len) = slot_of(buf, i as u16);
        if off == 0 && len == 0 {
            slot_idx = i;
            break;
        }
    }

    let slot_off = PAGE_HEADER_SIZE + slot_idx * SLOT_SIZE;
    put_u16(buf, slot_off, new_data_start as u16);
    put_u16(buf, slot_off + 2, row.len() as u16);

    if slot_idx == slot_count {
        put_u16(buf, SLOT_COUNT_OFF, (slot_count + 1) as u16);
    }

    update_checksum_of(buf);
    Some(slot_idx as SlotIndex)
}

fn delete_row_from(buf: &mut [u8], slot: SlotIndex) -> bool {
    if slot >= slot_count_of(buf) {
        return false;
    }
    let (off, len) = slot_of(buf, slot);
    if off == 0 && len == 0 {
        return false; // already deleted
    }
    let slot_off = PAGE_HEADER_SIZE + (slot as usize) * SLOT_SIZE;
    put_u16(buf, slot_off, 0);
    put_u16(buf, slot_off + 2, 0);
    update_checksum_of(buf);
    true
}

/// Result of an in-place update attempt on a slotted page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateResult {
    /// Row updated in place — same slot, same page.
    Updated,
    /// Slot not found or already deleted.
    NotFound,
    /// New row is too large for this page. Caller must do delete+insert
    /// on a different page (row migration).
    NotEnoughSpace,
}

/// Attempt to update a row in place (DB2-style).
///
/// - If `new_data` fits in the existing slot (same or smaller), overwrite directly.
/// - If `new_data` is larger but the page has enough free space, tombstone the
///   old slot, allocate new space from the free region, and repoint the slot.
/// - If the page doesn't have enough space, return `NotEnoughSpace`.
fn update_row_in(buf: &mut [u8], slot: SlotIndex, new_data: &[u8]) -> UpdateResult {
    if slot >= slot_count_of(buf) {
        return UpdateResult::NotFound;
    }
    let (old_off, old_len) = slot_of(buf, slot);
    if old_off == 0 && old_len == 0 {
        return UpdateResult::NotFound; // deleted
    }

    let new_len = new_data.len();
    let old_len_usize = old_len as usize;

    if new_len <= old_len_usize {
        // Case 1: new data fits in existing slot — overwrite in place.
        let start = old_off as usize;
        buf[start..start + new_len].copy_from_slice(new_data);
        // Update slot length (may be shorter — leaves dead bytes, but slot is accurate).
        let slot_off = PAGE_HEADER_SIZE + (slot as usize) * SLOT_SIZE;
        put_u16(buf, slot_off + 2, new_len as u16);
        update_checksum_of(buf);
        UpdateResult::Updated
    } else {
        // Case 2: new data is larger — check if page has enough free space.
        // We get back the old slot's space implicitly since we'll tombstone it,
        // but free_space_of() doesn't account for the old slot since it's still live.
        // Effective free = current free + old row bytes (which we'll reclaim).
        let effective_free = free_space_of(buf) + old_len_usize;
        if effective_free < new_len {
            return UpdateResult::NotEnoughSpace;
        }

        // Tombstone old slot (frees it for accounting but doesn't compact).
        let slot_entry_off = PAGE_HEADER_SIZE + (slot as usize) * SLOT_SIZE;
        put_u16(buf, slot_entry_off, 0);
        put_u16(buf, slot_entry_off + 2, 0);

        // Allocate new space from the free region (grows from end).
        let data_start = get_u16(buf, FREE_SPACE_OFF) as usize;
        let new_data_start = data_start - new_len;
        buf[new_data_start..new_data_start + new_len].copy_from_slice(new_data);
        put_u16(buf, FREE_SPACE_OFF, new_data_start as u16);

        // Repoint the same slot to the new location.
        put_u16(buf, slot_entry_off, new_data_start as u16);
        put_u16(buf, slot_entry_off + 2, new_len as u16);

        update_checksum_of(buf);
        UpdateResult::Updated
    }
}

/// Initialize a raw buffer as an empty data page with the given page_id.
pub(crate) fn init_page_buf(buf: &mut [u8], page_id: PageId) {
    buf.fill(0);
    put_u64(buf, PAGE_ID_OFF, page_id);
    buf[PAGE_TYPE_OFF] = PAGE_TYPE_DATA;
    put_u16(buf, FREE_SPACE_OFF, buf.len() as u16);
    put_u16(buf, SLOT_COUNT_OFF, 0);
    update_checksum_of(buf);
}

fn checksum_of(buf: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(&buf[..CHECKSUM_OFF]);
    h.update(&buf[CHECKSUM_OFF + 4..]);
    h.finalize()
}

pub(crate) fn verify_checksum_of(buf: &[u8]) -> Result<()> {
    let stored = get_u32(buf, CHECKSUM_OFF);
    let computed = checksum_of(buf);
    if stored != computed {
        return Err(Error::Corruption(format!(
            "page {} checksum mismatch: stored={stored:#010x}, computed={computed:#010x}",
            page_id_of(buf)
        )));
    }
    Ok(())
}

pub(crate) fn update_checksum_of(buf: &mut [u8]) {
    let crc = checksum_of(buf);
    put_u32(buf, CHECKSUM_OFF, crc);
}

// ── Traits: PageRead / PageWrite ──
// Shared method signatures with default impls that delegate to the free
// functions above. Each concrete type only provides buf() / buf_mut().

/// Read-only operations on any page buffer.
pub trait PageRead {
    /// Access the underlying byte buffer.
    fn buf(&self) -> &[u8];

    fn page_id(&self) -> PageId {
        page_id_of(self.buf())
    }

    fn page_type(&self) -> u8 {
        page_type_of(self.buf())
    }

    fn slot_count(&self) -> u16 {
        slot_count_of(self.buf())
    }

    fn page_size(&self) -> usize {
        self.buf().len()
    }

    fn free_space(&self) -> usize {
        free_space_of(self.buf())
    }

    fn read_row(&self, slot: SlotIndex) -> Option<&[u8]> {
        read_row_from(self.buf(), slot)
    }

    fn as_bytes(&self) -> &[u8] {
        self.buf()
    }
}

/// Mutable operations on a page buffer.
pub trait PageWrite: PageRead {
    /// Access the underlying byte buffer mutably.
    fn buf_mut(&mut self) -> &mut [u8];

    fn insert_row(&mut self, row: &[u8]) -> Option<SlotIndex> {
        insert_row_into(self.buf_mut(), row)
    }

    fn delete_row(&mut self, slot: SlotIndex) -> bool {
        delete_row_from(self.buf_mut(), slot)
    }

    fn update_row(&mut self, slot: SlotIndex, new_data: &[u8]) -> UpdateResult {
        update_row_in(self.buf_mut(), slot, new_data)
    }
}

// ── SlottedPage: owned buffer (used by HeapFile and standalone code) ──

/// A slotted page stored in an owned byte buffer.
///
/// Layout:
/// ```text
/// [Header 24 B][Slot directory →  ...  ← Row data][end]
/// ```
/// Slot directory grows forward from byte 24.
/// Row data grows backward from the end of the page.
#[derive(Debug)]
pub struct SlottedPage {
    buf: Vec<u8>,
}

impl SlottedPage {
    /// Create a new empty data page.
    pub fn new(page_id: PageId, page_size: usize) -> Self {
        let mut buf = vec![0u8; page_size];
        init_page_buf(&mut buf, page_id);
        Self { buf }
    }

    /// Wrap an existing byte buffer as a page (e.g. read from disk).
    pub fn from_bytes(buf: Vec<u8>) -> Result<Self> {
        if buf.len() < PAGE_HEADER_SIZE {
            return Err(Error::Corruption("page too small for header".into()));
        }
        verify_checksum_of(&buf)?;
        Ok(Self { buf })
    }

    /// Consume the page and return the inner buffer.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

impl PageRead for SlottedPage {
    fn buf(&self) -> &[u8] {
        &self.buf
    }
}

impl PageWrite for SlottedPage {
    fn buf_mut(&mut self) -> &mut [u8] {
        &mut self.buf
    }
}

// ── PageRef: read-only borrowed view (returned by buffer pool) ──

/// Read-only view of a page backed by a borrowed byte slice.
///
/// Created by `BufferPool::fetch_page`. The slice lives in the pool's
/// pre-allocated memory region.
#[derive(Debug)]
pub struct PageRef<'a> {
    buf: &'a [u8],
}

impl<'a> PageRef<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }
}

impl PageRead for PageRef<'_> {
    fn buf(&self) -> &[u8] {
        self.buf
    }
}

// ── PageMut: mutable borrowed view (returned by buffer pool) ──

/// Mutable view of a page backed by a borrowed byte slice.
///
/// Created by `BufferPool::fetch_page_mut` and `BufferPool::new_page`.
/// The slice lives in the pool's pre-allocated memory region.
#[derive(Debug)]
pub struct PageMut<'a> {
    buf: &'a mut [u8],
}

impl<'a> PageMut<'a> {
    pub(crate) fn new(buf: &'a mut [u8]) -> Self {
        Self { buf }
    }
}

impl PageRead for PageMut<'_> {
    fn buf(&self) -> &[u8] {
        self.buf
    }
}

impl PageWrite for PageMut<'_> {
    fn buf_mut(&mut self) -> &mut [u8] {
        self.buf
    }
}

// ── Little-endian byte helpers ──

fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
}

fn put_u16(buf: &mut [u8], off: usize, val: u16) {
    buf[off..off + 2].copy_from_slice(&val.to_le_bytes());
}

fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

fn put_u32(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

fn get_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn put_u64(buf: &mut [u8], off: usize, val: u64) {
    buf[off..off + 8].copy_from_slice(&val.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PAGE_SIZE: usize = 256;

    #[test]
    fn new_page_has_correct_header() {
        let p = SlottedPage::new(42, TEST_PAGE_SIZE);
        assert_eq!(p.page_id(), 42);
        assert_eq!(p.page_type(), PAGE_TYPE_DATA);
        assert_eq!(p.slot_count(), 0);
        assert_eq!(p.page_size(), TEST_PAGE_SIZE);
        assert!(p.free_space() > 0);
    }

    #[test]
    fn insert_and_read_row() {
        let mut p = SlottedPage::new(0, TEST_PAGE_SIZE);
        let data = b"hello world";
        let slot = p.insert_row(data).unwrap();
        assert_eq!(slot, 0);
        assert_eq!(p.slot_count(), 1);
        assert_eq!(p.read_row(slot), Some(data.as_slice()));
    }

    #[test]
    fn insert_multiple_rows() {
        let mut p = SlottedPage::new(0, TEST_PAGE_SIZE);
        let s0 = p.insert_row(b"row-0").unwrap();
        let s1 = p.insert_row(b"row-1").unwrap();
        let s2 = p.insert_row(b"row-2").unwrap();
        assert_eq!(s0, 0);
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
        assert_eq!(p.read_row(s0), Some(b"row-0".as_slice()));
        assert_eq!(p.read_row(s1), Some(b"row-1".as_slice()));
        assert_eq!(p.read_row(s2), Some(b"row-2".as_slice()));
    }

    #[test]
    fn delete_row_makes_it_unreadable() {
        let mut p = SlottedPage::new(0, TEST_PAGE_SIZE);
        let s = p.insert_row(b"gone").unwrap();
        assert!(p.delete_row(s));
        assert_eq!(p.read_row(s), None);
        // Double delete returns false.
        assert!(!p.delete_row(s));
    }

    #[test]
    fn deleted_slot_is_reused() {
        let mut p = SlottedPage::new(0, TEST_PAGE_SIZE);
        let s0 = p.insert_row(b"aaa").unwrap();
        let _s1 = p.insert_row(b"bbb").unwrap();
        p.delete_row(s0);
        // Next insert should reuse slot 0.
        let s2 = p.insert_row(b"ccc").unwrap();
        assert_eq!(s2, 0);
        assert_eq!(p.read_row(s2), Some(b"ccc".as_slice()));
    }

    #[test]
    fn overflow_returns_none() {
        let mut p = SlottedPage::new(0, TEST_PAGE_SIZE);
        // Try to insert a row that's too large for the page.
        let big = vec![0xFFu8; TEST_PAGE_SIZE];
        assert_eq!(p.insert_row(&big), None);
    }

    #[test]
    fn fill_page_until_full() {
        let mut p = SlottedPage::new(0, TEST_PAGE_SIZE);
        let row = [0xABu8; 20];
        let mut count = 0;
        while p.insert_row(&row).is_some() {
            count += 1;
        }
        // Should have inserted some rows but not infinite.
        assert!(count > 0);
        assert!(count < TEST_PAGE_SIZE / 20);
    }

    #[test]
    fn roundtrip_through_bytes() {
        let mut p = SlottedPage::new(7, TEST_PAGE_SIZE);
        p.insert_row(b"persist me").unwrap();
        let raw = p.into_bytes();
        let p2 = SlottedPage::from_bytes(raw).unwrap();
        assert_eq!(p2.page_id(), 7);
        assert_eq!(p2.read_row(0), Some(b"persist me".as_slice()));
    }

    #[test]
    fn corrupted_checksum_is_detected() {
        let mut p = SlottedPage::new(0, TEST_PAGE_SIZE);
        p.insert_row(b"data").unwrap();
        let mut raw = p.into_bytes();
        // Flip a byte in the row data area.
        let last = raw.len() - 1;
        raw[last] ^= 0xFF;
        let err = SlottedPage::from_bytes(raw).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn read_out_of_range_slot_returns_none() {
        let p = SlottedPage::new(0, TEST_PAGE_SIZE);
        assert_eq!(p.read_row(0), None);
        assert_eq!(p.read_row(99), None);
    }

    // ── update_row tests ──

    #[test]
    fn update_row_same_size() {
        let mut p = SlottedPage::new(0, TEST_PAGE_SIZE);
        let s = p.insert_row(b"hello").unwrap();
        assert_eq!(p.update_row(s, b"world"), UpdateResult::Updated);
        assert_eq!(p.read_row(s), Some(b"world".as_slice()));
    }

    #[test]
    fn update_row_smaller() {
        let mut p = SlottedPage::new(0, TEST_PAGE_SIZE);
        let s = p.insert_row(b"long data here").unwrap();
        assert_eq!(p.update_row(s, b"short"), UpdateResult::Updated);
        assert_eq!(p.read_row(s), Some(b"short".as_slice()));
    }

    #[test]
    fn update_row_larger_with_space() {
        let mut p = SlottedPage::new(0, TEST_PAGE_SIZE);
        let s = p.insert_row(b"tiny").unwrap();
        assert_eq!(
            p.update_row(s, b"a much longer replacement row"),
            UpdateResult::Updated
        );
        assert_eq!(
            p.read_row(s),
            Some(b"a much longer replacement row".as_slice())
        );
        // Slot index is preserved.
        assert_eq!(s, 0);
    }

    #[test]
    fn update_row_not_enough_space() {
        let mut p = SlottedPage::new(0, TEST_PAGE_SIZE);
        let s = p.insert_row(b"small").unwrap();
        // Fill page nearly full.
        let big = vec![0xABu8; TEST_PAGE_SIZE / 2];
        let _ = p.insert_row(&big);
        let _ = p.insert_row(&big);
        // Try to update the small row to something huge.
        let huge = vec![0xFFu8; TEST_PAGE_SIZE];
        assert_eq!(p.update_row(s, &huge), UpdateResult::NotEnoughSpace);
        // Original row should still be intact (was tombstoned only if we entered case 2).
        // Actually with NotEnoughSpace in case 2 path, we return before tombstoning.
        // Let's verify the row is still readable.
        assert_eq!(p.read_row(s), Some(b"small".as_slice()));
    }

    #[test]
    fn update_row_deleted_returns_not_found() {
        let mut p = SlottedPage::new(0, TEST_PAGE_SIZE);
        let s = p.insert_row(b"doomed").unwrap();
        p.delete_row(s);
        assert_eq!(p.update_row(s, b"nope"), UpdateResult::NotFound);
    }

    #[test]
    fn update_row_out_of_range_returns_not_found() {
        let mut p = SlottedPage::new(0, TEST_PAGE_SIZE);
        assert_eq!(p.update_row(99, b"nope"), UpdateResult::NotFound);
    }

    #[test]
    fn update_row_preserves_other_rows() {
        let mut p = SlottedPage::new(0, TEST_PAGE_SIZE);
        let s0 = p.insert_row(b"row-0").unwrap();
        let s1 = p.insert_row(b"row-1").unwrap();
        let s2 = p.insert_row(b"row-2").unwrap();
        // Update only row-1.
        assert_eq!(p.update_row(s1, b"UPDATED"), UpdateResult::Updated);
        assert_eq!(p.read_row(s0), Some(b"row-0".as_slice()));
        assert_eq!(p.read_row(s1), Some(b"UPDATED".as_slice()));
        assert_eq!(p.read_row(s2), Some(b"row-2".as_slice()));
    }
}
