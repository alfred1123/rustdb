use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use crate::error::{Error, Result};

/// A PostgreSQL-style Free Space Map implemented as a binary max-heap.
///
/// Each leaf represents one data page and stores a 1-byte category (0–255)
/// encoding the available free space. Internal nodes store the max of their
/// two children. Finding a page with at least N bytes free is an O(log P)
/// root-to-leaf walk where P = page count.
///
/// ## Category Encoding
///
/// ```text
/// category = free_bytes * 256 / page_size
/// free_bytes ≈ category * page_size / 256
/// ```
///
/// For 4096-byte pages each category step ≈ 16 bytes.
/// Category 255 = nearly empty page, category 0 = full.
///
/// ## Tree Layout (array-based binary heap)
///
/// ```text
/// Index 0: root (max of entire file)
/// Index 1, 2: children of root
/// Index 3..6: grandchildren
/// ...
/// Leaf nodes start at index `leaf_offset`.
/// Leaf at index `leaf_offset + pid` corresponds to data page `pid`.
/// ```
pub struct FreeSpaceMap {
    /// Array-based binary max-heap. Internal nodes at 0..leaf_offset,
    /// leaf nodes at leaf_offset..leaf_offset+leaf_count.
    tree: Vec<u8>,
    /// Index where leaf nodes start in `tree`.
    leaf_offset: usize,
    /// Number of leaf slots (rounded up to next power of 2).
    leaf_count: usize,
    /// Number of actual data pages tracked.
    page_count: usize,
    /// Page size for category encoding.
    page_size: usize,
}

impl FreeSpaceMap {
    /// Create a new FSM for the given page count and page size.
    ///
    /// Existing pages are initialised optimistically to category 255
    /// (maximum free space). The first real insert will correct each
    /// page's category to the actual value.
    pub fn new(page_count: usize, page_size: usize) -> Self {
        let leaf_count = page_count.next_power_of_two().max(1);
        let leaf_offset = leaf_count - 1;
        let tree_size = 2 * leaf_count - 1;
        let mut tree = vec![0u8; tree_size];

        // Initialise leaf nodes for existing pages optimistically.
        let max_cat = Self::bytes_to_cat_static(page_size, page_size);
        for pid in 0..page_count {
            tree[leaf_offset + pid] = max_cat;
        }

        let mut fsm = Self {
            tree,
            leaf_offset,
            leaf_count,
            page_count,
            page_size,
        };
        // Build internal nodes bottom-up.
        fsm.rebuild_internal();
        fsm
    }

    /// Number of data pages tracked.
    pub fn page_count(&self) -> usize {
        self.page_count
    }

    /// Find a page with at least `needed` bytes free. Returns `Some(page_id)`
    /// or `None` if no page qualifies. **O(log P).**
    pub fn search(&self, needed: usize) -> Option<usize> {
        if self.page_count == 0 {
            return None;
        }
        let target_cat = self.bytes_to_cat(needed);
        // Root doesn't have enough → no page qualifies.
        if self.tree[0] < target_cat {
            return None;
        }
        // Walk root to leaf, preferring left child.
        let mut idx = 0;
        while idx < self.leaf_offset {
            let left = 2 * idx + 1;
            let right = 2 * idx + 2;
            if left < self.tree.len() && self.tree[left] >= target_cat {
                idx = left;
            } else if right < self.tree.len() && self.tree[right] >= target_cat {
                idx = right;
            } else {
                // Should not happen if root check passed — defensive.
                return None;
            }
        }
        let pid = idx - self.leaf_offset;
        if pid < self.page_count {
            Some(pid)
        } else {
            None
        }
    }

    /// Update the free space for `page_id` after an insert or delete.
    /// Propagates the change up to the root. **O(log P).**
    pub fn update(&mut self, page_id: usize, free_bytes: usize) {
        if page_id >= self.page_count {
            return;
        }
        let cat = self.bytes_to_cat(free_bytes);
        let leaf_idx = self.leaf_offset + page_id;
        self.tree[leaf_idx] = cat;
        self.bubble_up(leaf_idx);
    }

    /// Extend the FSM to track `new_count` pages. New pages are initialised
    /// optimistically to max category. If the tree needs to grow, a new
    /// tree is allocated and internal nodes rebuilt.
    pub fn extend(&mut self, new_count: usize) {
        if new_count <= self.page_count {
            return;
        }
        let new_leaf_count = new_count.next_power_of_two().max(1);
        if new_leaf_count <= self.leaf_count {
            // Tree is large enough — just initialise new leaves.
            let max_cat = self.bytes_to_cat(self.page_size);
            for pid in self.page_count..new_count {
                self.tree[self.leaf_offset + pid] = max_cat;
            }
            self.page_count = new_count;
            self.rebuild_internal();
            return;
        }
        // Need a bigger tree.
        let new_leaf_offset = new_leaf_count - 1;
        let new_tree_size = 2 * new_leaf_count - 1;
        let mut new_tree = vec![0u8; new_tree_size];
        // Copy existing leaf values.
        for pid in 0..self.page_count {
            new_tree[new_leaf_offset + pid] = self.tree[self.leaf_offset + pid];
        }
        // Optimistically initialise new pages.
        let max_cat = Self::bytes_to_cat_static(self.page_size, self.page_size);
        for pid in self.page_count..new_count {
            new_tree[new_leaf_offset + pid] = max_cat;
        }
        self.tree = new_tree;
        self.leaf_offset = new_leaf_offset;
        self.leaf_count = new_leaf_count;
        self.page_count = new_count;
        self.rebuild_internal();
    }

    /// Persist the FSM to a `.FSM` file.
    ///
    /// Format: `[page_size: u32 LE][page_count: u32 LE][leaf categories: page_count bytes]`
    ///
    /// Only leaf data is written — the internal nodes are rebuilt on load.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.write_all(&(self.page_size as u32).to_le_bytes())?;
        file.write_all(&(self.page_count as u32).to_le_bytes())?;
        for pid in 0..self.page_count {
            file.write_all(&[self.tree[self.leaf_offset + pid]])?;
        }
        file.flush()?;
        Ok(())
    }

    /// Load an FSM from a `.FSM` file. Returns `None` if the file does not
    /// exist, so callers can fall back to optimistic initialisation.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let mut file = File::open(path)?;
        let mut header = [0u8; 8];
        if file.read_exact(&mut header).is_err() {
            return Ok(None);
        }
        let page_size = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
        let page_count = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;

        if page_size == 0 {
            return Err(Error::Corruption("FSM file has page_size=0".into()));
        }

        let leaf_count = page_count.next_power_of_two().max(1);
        let leaf_offset = leaf_count - 1;
        let tree_size = 2 * leaf_count - 1;
        let mut tree = vec![0u8; tree_size];

        let mut leaf_data = vec![0u8; page_count];
        file.read_exact(&mut leaf_data)?;
        for pid in 0..page_count {
            tree[leaf_offset + pid] = leaf_data[pid];
        }

        let mut fsm = Self {
            tree,
            leaf_offset,
            leaf_count,
            page_count,
            page_size,
        };
        fsm.rebuild_internal();
        Ok(Some(fsm))
    }

    // ── Internal helpers ──

    /// Convert free bytes to a 1-byte category.
    fn bytes_to_cat(&self, free_bytes: usize) -> u8 {
        Self::bytes_to_cat_static(self.page_size, free_bytes)
    }

    fn bytes_to_cat_static(page_size: usize, free_bytes: usize) -> u8 {
        if page_size == 0 {
            return 0;
        }
        let cat = free_bytes * 256 / page_size;
        cat.min(255) as u8
    }

    /// Propagate a leaf change up to the root.
    fn bubble_up(&mut self, mut idx: usize) {
        while idx > 0 {
            let parent = (idx - 1) / 2;
            let left = 2 * parent + 1;
            let right = 2 * parent + 2;
            let left_val = self.tree.get(left).copied().unwrap_or(0);
            let right_val = self.tree.get(right).copied().unwrap_or(0);
            let new_val = left_val.max(right_val);
            if self.tree[parent] == new_val {
                break; // no change — stop early.
            }
            self.tree[parent] = new_val;
            idx = parent;
        }
    }

    /// Rebuild all internal nodes from leaf values (bottom-up).
    fn rebuild_internal(&mut self) {
        if self.leaf_offset == 0 {
            return;
        }
        for i in (0..self.leaf_offset).rev() {
            let left = 2 * i + 1;
            let right = 2 * i + 2;
            let left_val = self.tree.get(left).copied().unwrap_or(0);
            let right_val = self.tree.get(right).copied().unwrap_or(0);
            self.tree[i] = left_val.max(right_val);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_fsm_returns_none() {
        let fsm = FreeSpaceMap::new(0, 4096);
        assert_eq!(fsm.search(1), None);
    }

    #[test]
    fn single_page_found() {
        let fsm = FreeSpaceMap::new(1, 4096);
        // Page 0 is optimistically full-free.
        assert_eq!(fsm.search(100), Some(0));
    }

    #[test]
    fn search_skips_full_pages() {
        let mut fsm = FreeSpaceMap::new(4, 4096);
        // Mark pages 0, 1, 2 as full.
        fsm.update(0, 0);
        fsm.update(1, 0);
        fsm.update(2, 0);
        // Page 3 still has space.
        assert_eq!(fsm.search(100), Some(3));
    }

    #[test]
    fn search_returns_none_when_all_full() {
        let mut fsm = FreeSpaceMap::new(4, 4096);
        for pid in 0..4 {
            fsm.update(pid, 0);
        }
        assert_eq!(fsm.search(100), None);
    }

    #[test]
    fn update_propagates_to_root() {
        let mut fsm = FreeSpaceMap::new(4, 4096);
        for pid in 0..4 {
            fsm.update(pid, 0);
        }
        assert_eq!(fsm.tree[0], 0); // root is 0.

        fsm.update(2, 2048);
        assert!(fsm.tree[0] > 0); // root now reflects page 2.
        assert_eq!(fsm.search(100), Some(2));
    }

    #[test]
    fn extend_grows_tree() {
        let mut fsm = FreeSpaceMap::new(2, 4096);
        fsm.update(0, 0);
        fsm.update(1, 0);
        assert_eq!(fsm.search(100), None);

        fsm.extend(5);
        assert_eq!(fsm.page_count(), 5);
        // New pages are optimistic — should find one.
        let found = fsm.search(100);
        assert!(found.is_some());
        let pid = found.unwrap();
        assert!(pid >= 2 && pid < 5);
    }

    #[test]
    fn extend_within_existing_capacity() {
        let mut fsm = FreeSpaceMap::new(2, 256);
        assert_eq!(fsm.leaf_count, 2);
        // Extend but stay within leaf_count.
        fsm.update(0, 0);
        fsm.update(1, 0);
        // leaf_count is 2, so extending to 2 does nothing.
        fsm.extend(2);
        assert_eq!(fsm.page_count(), 2);
    }

    #[test]
    fn category_encoding() {
        // 4096-byte pages: step = 16 bytes.
        let fsm = FreeSpaceMap::new(1, 4096);
        assert_eq!(fsm.bytes_to_cat(0), 0);
        assert_eq!(fsm.bytes_to_cat(16), 1);
        assert_eq!(fsm.bytes_to_cat(4096), 255);
        assert_eq!(fsm.bytes_to_cat(2048), 128);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join("rqdb_fsm_roundtrip");
        let _ = std::fs::create_dir_all(&dir);
        let fsm_path = dir.join("TEST.FSM");

        let mut fsm = FreeSpaceMap::new(8, 4096);
        fsm.update(0, 0);
        fsm.update(3, 1024);
        fsm.update(7, 4000);
        fsm.save(&fsm_path).unwrap();

        let loaded = FreeSpaceMap::load(&fsm_path).unwrap().unwrap();
        assert_eq!(loaded.page_count(), 8);
        assert_eq!(loaded.search(100), fsm.search(100));
        // Page 0 is full — should not be returned.
        assert_ne!(loaded.search(100), Some(0));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_nonexistent_returns_none() {
        let path = std::env::temp_dir().join("rqdb_fsm_no_such_file.FSM");
        let _ = std::fs::remove_file(&path);
        let result = FreeSpaceMap::load(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn prefers_left_child() {
        // When two pages have equal space, search should return the
        // lower page_id (left child preference).
        let fsm = FreeSpaceMap::new(4, 4096);
        // All pages are optimistic max — search should return page 0.
        assert_eq!(fsm.search(100), Some(0));
    }
}
