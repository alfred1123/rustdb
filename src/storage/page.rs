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

/// A slotted page stored in a fixed-size byte buffer.
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
        put_u64(&mut buf, PAGE_ID_OFF, page_id);
        buf[PAGE_TYPE_OFF] = PAGE_TYPE_DATA;
        // free_space initially points to the end of the page
        put_u16(&mut buf, FREE_SPACE_OFF, page_size as u16);
        put_u16(&mut buf, SLOT_COUNT_OFF, 0);
        let mut page = Self { buf };
        page.update_checksum();
        page
    }

    /// Wrap an existing byte buffer as a page (e.g. read from disk).
    pub fn from_bytes(buf: Vec<u8>) -> Result<Self> {
        if buf.len() < PAGE_HEADER_SIZE {
            return Err(Error::Corruption("page too small for header".into()));
        }
        let page = Self { buf };
        page.verify_checksum()?;
        Ok(page)
    }

    // ── Header accessors ──

    pub fn page_id(&self) -> PageId {
        get_u64(&self.buf, PAGE_ID_OFF)
    }

    pub fn page_type(&self) -> u8 {
        self.buf[PAGE_TYPE_OFF]
    }

    pub fn slot_count(&self) -> u16 {
        get_u16(&self.buf, SLOT_COUNT_OFF)
    }

    pub fn page_size(&self) -> usize {
        self.buf.len()
    }

    /// Usable free space available for a new row (including its slot entry).
    pub fn free_space(&self) -> usize {
        let dir_end = PAGE_HEADER_SIZE + (self.slot_count() as usize) * SLOT_SIZE;
        let data_start = get_u16(&self.buf, FREE_SPACE_OFF) as usize;
        if data_start <= dir_end + SLOT_SIZE {
            0
        } else {
            data_start - dir_end - SLOT_SIZE
        }
    }

    /// Insert a row into the page. Returns the slot index, or `None` if the
    /// row does not fit.
    pub fn insert_row(&mut self, row: &[u8]) -> Option<SlotIndex> {
        if self.free_space() < row.len() {
            return None;
        }

        // Allocate row space (grows from end toward header).
        let data_start = get_u16(&self.buf, FREE_SPACE_OFF) as usize;
        let new_data_start = data_start - row.len();
        self.buf[new_data_start..new_data_start + row.len()].copy_from_slice(row);
        put_u16(&mut self.buf, FREE_SPACE_OFF, new_data_start as u16);

        // Check for a reusable deleted slot.
        let slot_count = self.slot_count() as usize;
        let mut slot_idx = slot_count; // default: append new slot
        for i in 0..slot_count {
            let (off, len) = self.slot(i as u16);
            if off == 0 && len == 0 {
                slot_idx = i;
                break;
            }
        }

        let slot_off = PAGE_HEADER_SIZE + slot_idx * SLOT_SIZE;
        put_u16(&mut self.buf, slot_off, new_data_start as u16);
        put_u16(&mut self.buf, slot_off + 2, row.len() as u16);

        if slot_idx == slot_count {
            put_u16(&mut self.buf, SLOT_COUNT_OFF, (slot_count + 1) as u16);
        }

        self.update_checksum();
        Some(slot_idx as SlotIndex)
    }

    /// Read the row bytes at the given slot. Returns `None` if the slot is
    /// deleted or out of range.
    pub fn read_row(&self, slot: SlotIndex) -> Option<&[u8]> {
        if slot >= self.slot_count() {
            return None;
        }
        let (off, len) = self.slot(slot);
        if off == 0 && len == 0 {
            return None; // deleted
        }
        Some(&self.buf[off as usize..(off + len) as usize])
    }

    /// Mark a slot as deleted. The space is not reclaimed until the page is
    /// compacted.
    pub fn delete_row(&mut self, slot: SlotIndex) -> bool {
        if slot >= self.slot_count() {
            return false;
        }
        let (off, len) = self.slot(slot);
        if off == 0 && len == 0 {
            return false; // already deleted
        }
        let slot_off = PAGE_HEADER_SIZE + (slot as usize) * SLOT_SIZE;
        put_u16(&mut self.buf, slot_off, 0);
        put_u16(&mut self.buf, slot_off + 2, 0);
        self.update_checksum();
        true
    }

    /// Return the raw page buffer (for writing to disk).
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Consume the page and return the inner buffer.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    // ── Internal helpers ──

    /// Read slot (offset, length) for a given index.
    fn slot(&self, idx: SlotIndex) -> (u16, u16) {
        let base = PAGE_HEADER_SIZE + (idx as usize) * SLOT_SIZE;
        (get_u16(&self.buf, base), get_u16(&self.buf, base + 2))
    }

    fn checksum_range(&self) -> u32 {
        // Checksum covers everything except the checksum field itself.
        let mut h = crc32fast::Hasher::new();
        h.update(&self.buf[..CHECKSUM_OFF]);
        h.update(&self.buf[CHECKSUM_OFF + 4..]);
        h.finalize()
    }

    fn update_checksum(&mut self) {
        let crc = self.checksum_range();
        put_u32(&mut self.buf, CHECKSUM_OFF, crc);
    }

    fn verify_checksum(&self) -> Result<()> {
        let stored = get_u32(&self.buf, CHECKSUM_OFF);
        let computed = self.checksum_range();
        if stored != computed {
            return Err(Error::Corruption(format!(
                "page {} checksum mismatch: stored={stored:#010x}, computed={computed:#010x}",
                self.page_id()
            )));
        }
        Ok(())
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
}
