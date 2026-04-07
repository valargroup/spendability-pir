//! Flat append-only store of decryption PIR leaves with per-block rollback.
//!
//! Stores [`DecryptionLeaf`] values indexed by global Orchard tree position,
//! organized into the same shard/sub-shard geometry as the witness PIR
//! database. Unlike [`CommitmentTreeDb`], no Merkle hashing is performed —
//! this is a simple position-indexed array with block-level undo support.
//!
//! # Windowed mode
//!
//! When constructed via [`DecryptionDb::with_offset`], only leaves within the
//! PIR window are stored. The offset must be shard-aligned so that sub-shard
//! boundaries stay consistent with the witness PIR database.
//!
//! # Operations
//!
//! - [`DecryptionDb::append_leaves`] — extend the store with a block's leaves
//! - [`DecryptionDb::rollback_to`] — handle chain reorgs
//! - [`DecryptionDb::subshard_leaves`] — 256 leaves for a given sub-shard
//! - [`DecryptionDb::build_pir_db`] — row-major bytes for YPIR setup
//! - Snapshot/restore via [`DecryptionDb::to_snapshot`] / [`DecryptionDb::from_snapshot`]

pub mod snapshot;

use decryption_types::*;

/// Per-block record tracking how many leaves each block contributed.
#[derive(Debug, Clone)]
pub struct BlockRecord {
    pub height: u64,
    pub hash: [u8; 32],
    pub num_leaves: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("snapshot corrupted: {reason}")]
    SnapshotCorrupted { reason: String },
}

/// In-memory flat store of decryption PIR leaves.
///
/// Leaves are stored in append order. `leaves[i]` corresponds to global
/// tree position `leaf_offset + i`. Positions beyond the frontier are
/// filled with [`DecryptionLeaf::EMPTY`] when building the PIR database.
pub struct DecryptionDb {
    leaves: Vec<DecryptionLeaf>,
    blocks: Vec<BlockRecord>,
    leaf_offset: u64,
}

impl DecryptionDb {
    pub fn new() -> Self {
        Self {
            leaves: Vec::new(),
            blocks: Vec::new(),
            leaf_offset: 0,
        }
    }

    /// Create a store that only tracks leaves starting at `leaf_offset`.
    ///
    /// `leaf_offset` must be shard-aligned (`leaf_offset % SHARD_LEAVES == 0`)
    /// to keep sub-shard boundaries aligned with the witness PIR database.
    pub fn with_offset(leaf_offset: u64) -> Self {
        debug_assert_eq!(
            leaf_offset as usize % SHARD_LEAVES,
            0,
            "leaf_offset must be shard-aligned"
        );
        Self {
            leaves: Vec::new(),
            blocks: Vec::new(),
            leaf_offset,
        }
    }

    /// Total number of leaves in the global tree (offset + local leaves).
    pub fn tree_size(&self) -> u64 {
        self.leaf_offset + self.leaves.len() as u64
    }

    pub fn latest_height(&self) -> Option<u64> {
        self.blocks.last().map(|b| b.height)
    }

    pub fn latest_block_hash(&self) -> Option<[u8; 32]> {
        self.blocks.last().map(|b| b.hash)
    }

    pub fn leaf_offset(&self) -> u64 {
        self.leaf_offset
    }

    pub fn leaves(&self) -> &[DecryptionLeaf] {
        &self.leaves
    }

    pub fn blocks(&self) -> &[BlockRecord] {
        &self.blocks
    }

    /// Number of populated shards in the global tree.
    pub fn populated_shards(&self) -> u32 {
        let total = self.tree_size();
        if total == 0 {
            0
        } else {
            ((total as usize - 1) / SHARD_LEAVES + 1) as u32
        }
    }

    /// First shard index in the PIR window.
    pub fn window_start_shard(&self) -> u32 {
        (self.leaf_offset as usize / SHARD_LEAVES) as u32
    }

    /// Number of shards with local leaf data, capped at [`L0_MAX_SHARDS`].
    pub fn window_shard_count(&self) -> u32 {
        if self.leaves.is_empty() {
            return 0;
        }
        let first = self.window_start_shard();
        let last = ((self.tree_size() as usize - 1) / SHARD_LEAVES) as u32;
        (last - first + 1).min(L0_MAX_SHARDS as u32)
    }

    // -- Mutation --

    /// Append decryption leaves from a newly ingested block.
    pub fn append_leaves(&mut self, height: u64, hash: [u8; 32], leaves: &[DecryptionLeaf]) {
        self.leaves.extend_from_slice(leaves);
        self.blocks.push(BlockRecord {
            height,
            hash,
            num_leaves: leaves.len() as u32,
        });
    }

    /// Roll back all blocks with height strictly greater than `target_height`.
    pub fn rollback_to(&mut self, target_height: u64) {
        let mut to_remove: usize = 0;
        while let Some(last) = self.blocks.last() {
            if last.height > target_height {
                to_remove += last.num_leaves as usize;
                self.blocks.pop();
            } else {
                break;
            }
        }
        let new_len = self.leaves.len().saturating_sub(to_remove);
        self.leaves.truncate(new_len);
    }

    // -- Leaf queries --

    /// Retrieve the 256 decryption leaves for a sub-shard.
    ///
    /// Positions beyond the frontier are filled with [`DecryptionLeaf::EMPTY`].
    pub fn subshard_leaves(&self, shard_idx: u32, subshard_idx: u8) -> Vec<DecryptionLeaf> {
        let global_start =
            (shard_idx as usize) * SHARD_LEAVES + (subshard_idx as usize) * SUBSHARD_LEAVES;

        (0..SUBSHARD_LEAVES)
            .map(|i| {
                let global_pos = global_start + i;
                if global_pos < self.leaf_offset as usize {
                    return DecryptionLeaf::EMPTY;
                }
                let local = global_pos - self.leaf_offset as usize;
                if local < self.leaves.len() {
                    self.leaves[local]
                } else {
                    DecryptionLeaf::EMPTY
                }
            })
            .collect()
    }

    // -- PIR database --

    /// Build the PIR database as row-major bytes.
    ///
    /// Each row is one sub-shard: 256 leaves x 116 bytes = 29,696 bytes.
    /// Rows beyond the window are zero-filled (matching [`DecryptionLeaf::EMPTY`]).
    pub fn build_pir_db(&self) -> Vec<u8> {
        let window_start = self.window_start_shard();
        let window_count = self.window_shard_count();

        let mut db = Vec::with_capacity(DECRYPT_DB_BYTES);

        for i in 0..window_count {
            let shard_idx = window_start + i;
            for ss in 0..SUBSHARDS_PER_SHARD {
                let leaves = self.subshard_leaves(shard_idx, ss as u8);
                for leaf in &leaves {
                    db.extend_from_slice(&leaf.to_bytes());
                }
            }
        }

        db.resize(DECRYPT_DB_BYTES, 0u8);
        db
    }
}

impl Default for DecryptionDb {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_leaf(byte: u8) -> DecryptionLeaf {
        DecryptionLeaf {
            nf: [byte; 32],
            ephemeral_key: [byte.wrapping_add(1); 32],
            ciphertext: [byte.wrapping_add(2); 52],
        }
    }

    #[test]
    fn empty_db() {
        let db = DecryptionDb::new();
        assert_eq!(db.tree_size(), 0);
        assert_eq!(db.populated_shards(), 0);
        assert!(db.latest_height().is_none());
        assert!(db.latest_block_hash().is_none());
        assert_eq!(db.window_shard_count(), 0);
    }

    #[test]
    fn append_and_track() {
        let mut db = DecryptionDb::new();
        db.append_leaves(100, [1u8; 32], &[make_leaf(0xAA), make_leaf(0xBB)]);
        assert_eq!(db.tree_size(), 2);
        assert_eq!(db.populated_shards(), 1);
        assert_eq!(db.latest_height(), Some(100));
        assert_eq!(db.latest_block_hash(), Some([1u8; 32]));

        db.append_leaves(101, [2u8; 32], &[make_leaf(0xCC)]);
        assert_eq!(db.tree_size(), 3);
        assert_eq!(db.latest_height(), Some(101));
    }

    #[test]
    fn rollback_removes_blocks_and_leaves() {
        let mut db = DecryptionDb::new();
        db.append_leaves(100, [1u8; 32], &[make_leaf(1), make_leaf(2)]);
        db.append_leaves(101, [2u8; 32], &[make_leaf(3)]);
        db.append_leaves(102, [3u8; 32], &[make_leaf(4), make_leaf(5)]);
        assert_eq!(db.tree_size(), 5);

        db.rollback_to(100);
        assert_eq!(db.tree_size(), 2);
        assert_eq!(db.latest_height(), Some(100));
        assert_eq!(db.blocks.len(), 1);
    }

    #[test]
    fn rollback_all() {
        let mut db = DecryptionDb::new();
        db.append_leaves(100, [1u8; 32], &[make_leaf(1)]);
        db.append_leaves(101, [2u8; 32], &[make_leaf(2)]);

        db.rollback_to(0);
        assert_eq!(db.tree_size(), 0);
        assert!(db.blocks.is_empty());
        assert!(db.leaves.is_empty());
    }

    #[test]
    fn subshard_leaves_padding() {
        let mut db = DecryptionDb::new();
        db.append_leaves(100, [1u8; 32], &[make_leaf(1), make_leaf(2), make_leaf(3)]);

        let leaves = db.subshard_leaves(0, 0);
        assert_eq!(leaves.len(), SUBSHARD_LEAVES);
        assert_eq!(leaves[0], make_leaf(1));
        assert_eq!(leaves[1], make_leaf(2));
        assert_eq!(leaves[2], make_leaf(3));
        for leaf in &leaves[3..] {
            assert_eq!(*leaf, DecryptionLeaf::EMPTY);
        }
    }

    #[test]
    fn subshard_leaves_entirely_empty() {
        let db = DecryptionDb::new();
        let leaves = db.subshard_leaves(0, 0);
        assert_eq!(leaves.len(), SUBSHARD_LEAVES);
        for leaf in &leaves {
            assert_eq!(*leaf, DecryptionLeaf::EMPTY);
        }
    }

    #[test]
    fn pir_db_size_correct() {
        let mut db = DecryptionDb::new();
        db.append_leaves(100, [1u8; 32], &[make_leaf(1)]);
        let pir = db.build_pir_db();
        assert_eq!(pir.len(), DECRYPT_DB_BYTES);
    }

    #[test]
    fn pir_db_contains_leaf_data() {
        let mut db = DecryptionDb::new();
        let leaf = make_leaf(0xAB);
        db.append_leaves(100, [1u8; 32], &[leaf]);
        let pir = db.build_pir_db();

        let stored = DecryptionLeaf::from_bytes(&pir[..DECRYPT_LEAF_BYTES]).unwrap();
        assert_eq!(stored, leaf);
    }

    #[test]
    fn pir_db_empty_is_zeroed() {
        let db = DecryptionDb::new();
        let pir = db.build_pir_db();
        assert_eq!(pir.len(), DECRYPT_DB_BYTES);
        assert!(pir.iter().all(|&b| b == 0));
    }

    #[test]
    fn pir_db_row_layout() {
        let mut db = DecryptionDb::new();
        let leaves: Vec<DecryptionLeaf> = (0..SUBSHARD_LEAVES)
            .map(|i| make_leaf(i as u8))
            .collect();
        db.append_leaves(100, [1u8; 32], &leaves);
        let pir = db.build_pir_db();

        for i in 0..SUBSHARD_LEAVES {
            let offset = i * DECRYPT_LEAF_BYTES;
            let stored = DecryptionLeaf::from_bytes(&pir[offset..]).unwrap();
            assert_eq!(stored, leaves[i], "leaf {i} mismatch in PIR DB row");
        }
    }

    #[test]
    fn append_empty_block() {
        let mut db = DecryptionDb::new();
        db.append_leaves(100, [1u8; 32], &[make_leaf(1)]);
        db.append_leaves(101, [2u8; 32], &[]);
        assert_eq!(db.tree_size(), 1);
        assert_eq!(db.latest_height(), Some(101));
        assert_eq!(db.blocks.len(), 2);
    }

    #[test]
    fn rollback_then_reappend() {
        let mut db = DecryptionDb::new();
        db.append_leaves(100, [1u8; 32], &[make_leaf(1), make_leaf(2)]);
        db.append_leaves(101, [2u8; 32], &[make_leaf(3)]);

        db.rollback_to(100);
        assert_eq!(db.tree_size(), 2);

        db.append_leaves(101, [3u8; 32], &[make_leaf(4)]);
        assert_eq!(db.tree_size(), 3);
        assert_eq!(db.leaves[2], make_leaf(4));
    }

    // -- Windowed (offset) tests --

    #[test]
    fn with_offset_basic() {
        let offset = 2 * SHARD_LEAVES as u64;
        let db = DecryptionDb::with_offset(offset);
        assert_eq!(db.tree_size(), offset);
        assert_eq!(db.leaf_offset(), offset);
        assert_eq!(db.window_start_shard(), 2);
    }

    #[test]
    fn with_offset_append() {
        let offset = 3 * SHARD_LEAVES as u64;
        let mut db = DecryptionDb::with_offset(offset);
        db.append_leaves(100, [1u8; 32], &[make_leaf(1), make_leaf(2)]);

        assert_eq!(db.tree_size(), offset + 2);
        assert_eq!(db.populated_shards(), 4);
        assert_eq!(db.window_start_shard(), 3);
        assert_eq!(db.window_shard_count(), 1);
    }

    #[test]
    fn with_offset_pir_db_starts_at_window() {
        let offset = 2 * SHARD_LEAVES as u64;
        let mut db = DecryptionDb::with_offset(offset);
        let leaf = make_leaf(0xDD);
        db.append_leaves(100, [1u8; 32], &[leaf]);

        let pir = db.build_pir_db();
        assert_eq!(pir.len(), DECRYPT_DB_BYTES);
        let stored = DecryptionLeaf::from_bytes(&pir[..DECRYPT_LEAF_BYTES]).unwrap();
        assert_eq!(stored, leaf);
    }

    #[test]
    fn with_offset_subshard_before_window_is_empty() {
        let offset = 2 * SHARD_LEAVES as u64;
        let mut db = DecryptionDb::with_offset(offset);
        db.append_leaves(100, [1u8; 32], &[make_leaf(1)]);

        let leaves = db.subshard_leaves(0, 0);
        for leaf in &leaves {
            assert_eq!(*leaf, DecryptionLeaf::EMPTY);
        }
    }

    #[test]
    fn window_counts() {
        let mut db = DecryptionDb::new();
        assert_eq!(db.window_shard_count(), 0);

        db.append_leaves(100, [1u8; 32], &[make_leaf(1)]);
        assert_eq!(db.window_shard_count(), 1);
    }
}
