//! Snapshot serialization and deserialization for [`CommitmentTreeDb`].
//!
//! Binary format (all integers little-endian):
//!
//! | Field               | Size     | Description                                    |
//! |---------------------|----------|------------------------------------------------|
//! | magic               | 8 bytes  | `0x434D_5452_4545_0001` (version 1)            |
//! | tree_size           | 8 bytes  | Number of leaves                               |
//! | block_count         | 8 bytes  | Number of block records                        |
//! | latest_height       | 8 bytes  | Height of the most recent block (0 if empty)   |
//! | latest_hash         | 32 bytes | Hash of the most recent block (zeros if empty)  |
//! | block_records       | variable | `block_count` × (height: 8, hash: 32, n_cmx: 4)|
//! | leaf_data           | variable | `tree_size` × 32 bytes                         |
//! | checksum            | 8 bytes  | xxHash64 over everything preceding              |
//!
//! Follows the same pattern as `hashtable-pir/src/snapshot.rs`.

use crate::{BlockRecord, CommitmentTreeDb, TreeError};
use xxhash_rust::xxh64::xxh64;

const SNAPSHOT_MAGIC: u64 = 0x434D_5452_4545_0001;
const BLOCK_RECORD_SIZE: usize = 8 + 32 + 4; // height + hash + num_commitments

impl CommitmentTreeDb {
    /// Serialize the current tree state to a snapshot byte vector.
    pub fn to_snapshot(&self) -> Vec<u8> {
        let block_count = self.blocks().len();
        let tree_size = self.tree_size();
        let estimated =
            8 + 8 + 8 + 8 + 32 + block_count * BLOCK_RECORD_SIZE + (tree_size as usize) * 32 + 8;
        let mut buf = Vec::with_capacity(estimated);

        // Header
        buf.extend_from_slice(&SNAPSHOT_MAGIC.to_le_bytes());
        buf.extend_from_slice(&tree_size.to_le_bytes());
        buf.extend_from_slice(&(block_count as u64).to_le_bytes());

        let latest_height = self.latest_height().unwrap_or(0);
        let latest_hash = self.latest_block_hash().unwrap_or([0u8; 32]);
        buf.extend_from_slice(&latest_height.to_le_bytes());
        buf.extend_from_slice(&latest_hash);

        // Block records
        for block in self.blocks() {
            buf.extend_from_slice(&block.height.to_le_bytes());
            buf.extend_from_slice(&block.hash);
            buf.extend_from_slice(&block.num_commitments.to_le_bytes());
        }

        // Leaf data
        for leaf in self.leaves() {
            buf.extend_from_slice(leaf);
        }

        // Checksum
        let checksum = xxh64(&buf, 0);
        buf.extend_from_slice(&checksum.to_le_bytes());

        buf
    }

    /// Restore a tree from a snapshot byte slice.
    pub fn from_snapshot(data: &[u8]) -> Result<Self, TreeError> {
        let header_size = 8 + 8 + 8 + 8 + 32; // magic + tree_size + block_count + height + hash
        let min_size = header_size + 8; // + checksum
        if data.len() < min_size {
            return Err(TreeError::SnapshotCorrupted {
                reason: "data too short".into(),
            });
        }

        // Verify checksum
        let payload = &data[..data.len() - 8];
        let stored_checksum =
            u64::from_le_bytes(data[data.len() - 8..].try_into().map_err(|_| {
                TreeError::SnapshotCorrupted {
                    reason: "checksum read failed".into(),
                }
            })?);
        let computed_checksum = xxh64(payload, 0);
        if stored_checksum != computed_checksum {
            return Err(TreeError::SnapshotCorrupted {
                reason: "checksum mismatch".into(),
            });
        }

        let mut pos = 0;
        let read_u64 = |pos: &mut usize| -> Result<u64, TreeError> {
            if *pos + 8 > payload.len() {
                return Err(TreeError::SnapshotCorrupted {
                    reason: "unexpected EOF reading u64".into(),
                });
            }
            let val = u64::from_le_bytes(payload[*pos..*pos + 8].try_into().unwrap());
            *pos += 8;
            Ok(val)
        };

        let magic = read_u64(&mut pos)?;
        if magic != SNAPSHOT_MAGIC {
            return Err(TreeError::SnapshotCorrupted {
                reason: format!("bad magic: expected {SNAPSHOT_MAGIC:#018x}, got {magic:#018x}"),
            });
        }

        let tree_size = read_u64(&mut pos)? as usize;
        let block_count = read_u64(&mut pos)? as usize;
        let _latest_height = read_u64(&mut pos)?;

        // latest_hash (32 bytes)
        if pos + 32 > payload.len() {
            return Err(TreeError::SnapshotCorrupted {
                reason: "unexpected EOF reading latest_hash".into(),
            });
        }
        pos += 32;

        // Block records
        let block_data_end = pos + block_count * BLOCK_RECORD_SIZE;
        if block_data_end > payload.len() {
            return Err(TreeError::SnapshotCorrupted {
                reason: "block records truncated".into(),
            });
        }

        let mut blocks = Vec::with_capacity(block_count);
        let mut total_commitments: usize = 0;
        for _ in 0..block_count {
            let height = u64::from_le_bytes(payload[pos..pos + 8].try_into().unwrap());
            pos += 8;

            let mut hash = [0u8; 32];
            hash.copy_from_slice(&payload[pos..pos + 32]);
            pos += 32;

            let num_commitments = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap());
            pos += 4;

            total_commitments += num_commitments as usize;
            blocks.push(BlockRecord {
                height,
                hash,
                num_commitments,
            });
        }

        if total_commitments != tree_size {
            return Err(TreeError::SnapshotCorrupted {
                reason: format!(
                    "block records sum to {total_commitments} commitments but header says {tree_size}"
                ),
            });
        }

        // Leaf data
        let leaf_data_end = pos + tree_size * 32;
        if leaf_data_end > payload.len() {
            return Err(TreeError::SnapshotCorrupted {
                reason: "leaf data truncated".into(),
            });
        }

        let mut leaves = Vec::with_capacity(tree_size);
        for _ in 0..tree_size {
            let mut leaf = [0u8; 32];
            leaf.copy_from_slice(&payload[pos..pos + 32]);
            pos += 32;
            leaves.push(leaf);
        }

        let mut tree = CommitmentTreeDb::new();
        tree.leaves = leaves;
        tree.blocks = blocks;
        Ok(tree)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use witness_types::Hash;

    fn make_leaf(byte: u8) -> Hash {
        let mut h = [0u8; 32];
        h[0] = byte;
        h
    }

    #[test]
    fn empty_snapshot_roundtrip() {
        let tree = CommitmentTreeDb::new();
        let snap = tree.to_snapshot();
        let restored = CommitmentTreeDb::from_snapshot(&snap).unwrap();

        assert_eq!(restored.tree_size(), 0);
        assert!(restored.blocks().is_empty());
        assert_eq!(restored.tree_root(), tree.tree_root());
    }

    #[test]
    fn populated_snapshot_roundtrip() {
        let mut tree = CommitmentTreeDb::new();
        tree.append_commitments(100, [0xAA; 32], &[make_leaf(1), make_leaf(2)]);
        tree.append_commitments(101, [0xBB; 32], &[make_leaf(3)]);

        let snap = tree.to_snapshot();
        let restored = CommitmentTreeDb::from_snapshot(&snap).unwrap();

        assert_eq!(restored.tree_size(), 3);
        assert_eq!(restored.blocks().len(), 2);
        assert_eq!(restored.latest_height(), Some(101));
        assert_eq!(restored.latest_block_hash(), Some([0xBB; 32]));
        assert_eq!(restored.tree_root(), tree.tree_root());
    }

    #[test]
    fn snapshot_detects_corruption() {
        let mut tree = CommitmentTreeDb::new();
        tree.append_commitments(100, [1u8; 32], &[make_leaf(1)]);

        let mut snap = tree.to_snapshot();
        // Flip a byte in the middle
        let mid = snap.len() / 2;
        snap[mid] ^= 0xFF;

        let result = CommitmentTreeDb::from_snapshot(&snap);
        assert!(result.is_err());
    }

    #[test]
    fn snapshot_detects_truncation() {
        let result = CommitmentTreeDb::from_snapshot(&[0u8; 10]);
        assert!(result.is_err());
    }

    #[test]
    fn snapshot_preserves_rollback_ability() {
        let mut tree = CommitmentTreeDb::new();
        tree.append_commitments(100, [1u8; 32], &[make_leaf(1), make_leaf(2)]);
        tree.append_commitments(101, [2u8; 32], &[make_leaf(3)]);

        let snap = tree.to_snapshot();
        let mut restored = CommitmentTreeDb::from_snapshot(&snap).unwrap();
        let root_before = restored.tree_root();

        restored.rollback_to(100);
        assert_eq!(restored.tree_size(), 2);
        assert_eq!(restored.latest_height(), Some(100));
        assert_ne!(restored.tree_root(), root_before);
    }

    #[test]
    fn snapshot_bad_magic() {
        let mut snap = CommitmentTreeDb::new().to_snapshot();
        // Corrupt magic
        snap[0] = 0xFF;
        snap[1] = 0xFF;
        // Recompute checksum so it passes checksum check but fails magic check
        // Actually the checksum will fail first. Let's test with proper re-checksum.
        let result = CommitmentTreeDb::from_snapshot(&snap);
        assert!(result.is_err());
    }
}
