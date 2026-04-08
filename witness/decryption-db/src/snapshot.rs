//! Snapshot serialization and deserialization for [`DecryptionDb`].
//!
//! Binary format v1 (all integers little-endian):
//!
//! | Field             | Size     | Description                                     |
//! |-------------------|----------|-------------------------------------------------|
//! | magic             | 8 bytes  | `0x4445_4352_5950_0001`                         |
//! | tree_size         | 8 bytes  | Total global number of leaves                   |
//! | block_count       | 8 bytes  | Number of block records                         |
//! | latest_height     | 8 bytes  | Height of the most recent block (0 if empty)    |
//! | latest_hash       | 32 bytes | Hash of the most recent block (zeros if empty)  |
//! | leaf_offset       | 8 bytes  | Global position of first stored leaf             |
//! | block_records     | variable | `block_count` × (height:8 + hash:32 + n_leaves:4) |
//! | leaf_data         | variable | `local_leaves` × 116 bytes                      |
//! | checksum          | 8 bytes  | xxHash64 over everything preceding               |

use crate::{BlockRecord, DbError, DecryptionDb};
use decryption_types::{DecryptionLeaf, DECRYPT_LEAF_BYTES};
use xxhash_rust::xxh64::xxh64;

const SNAPSHOT_MAGIC_V1: u64 = 0x4445_4352_5950_0001;
const BLOCK_RECORD_SIZE: usize = 8 + 32 + 4;

impl DecryptionDb {
    pub fn to_snapshot(&self) -> Vec<u8> {
        let block_count = self.blocks.len();
        let local_leaves = self.leaves.len();
        let estimated = 8
            + 8
            + 8
            + 8
            + 32
            + 8
            + block_count * BLOCK_RECORD_SIZE
            + local_leaves * DECRYPT_LEAF_BYTES
            + 8;
        let mut buf = Vec::with_capacity(estimated);

        buf.extend_from_slice(&SNAPSHOT_MAGIC_V1.to_le_bytes());
        buf.extend_from_slice(&self.tree_size().to_le_bytes());
        buf.extend_from_slice(&(block_count as u64).to_le_bytes());

        let latest_height = self.latest_height().unwrap_or(0);
        let latest_hash = self.latest_block_hash().unwrap_or([0u8; 32]);
        buf.extend_from_slice(&latest_height.to_le_bytes());
        buf.extend_from_slice(&latest_hash);
        buf.extend_from_slice(&self.leaf_offset.to_le_bytes());

        for block in &self.blocks {
            buf.extend_from_slice(&block.height.to_le_bytes());
            buf.extend_from_slice(&block.hash);
            buf.extend_from_slice(&block.num_leaves.to_le_bytes());
        }

        for leaf in &self.leaves {
            buf.extend_from_slice(&leaf.to_bytes());
        }

        let checksum = xxh64(&buf, 0);
        buf.extend_from_slice(&checksum.to_le_bytes());

        buf
    }

    pub fn from_snapshot(data: &[u8]) -> Result<Self, DbError> {
        if data.len() < 16 {
            return Err(DbError::SnapshotCorrupted {
                reason: "data too short".into(),
            });
        }

        let payload = &data[..data.len() - 8];
        let stored_checksum =
            u64::from_le_bytes(data[data.len() - 8..].try_into().map_err(|_| {
                DbError::SnapshotCorrupted {
                    reason: "checksum read failed".into(),
                }
            })?);
        if xxh64(payload, 0) != stored_checksum {
            return Err(DbError::SnapshotCorrupted {
                reason: "checksum mismatch".into(),
            });
        }

        let mut pos = 0;

        let magic = read_u64(payload, &mut pos)?;
        if magic != SNAPSHOT_MAGIC_V1 {
            return Err(DbError::SnapshotCorrupted {
                reason: format!("bad magic: {magic:#018x}"),
            });
        }

        let tree_size = read_u64(payload, &mut pos)? as usize;
        let block_count = read_u64(payload, &mut pos)? as usize;
        let _latest_height = read_u64(payload, &mut pos)?;

        if pos + 32 > payload.len() {
            return Err(DbError::SnapshotCorrupted {
                reason: "unexpected EOF reading latest_hash".into(),
            });
        }
        pos += 32;

        let leaf_offset = read_u64(payload, &mut pos)?;

        if tree_size < leaf_offset as usize {
            return Err(DbError::SnapshotCorrupted {
                reason: format!("tree_size ({tree_size}) < leaf_offset ({leaf_offset})"),
            });
        }

        let (blocks, total_leaves) = read_block_records(payload, &mut pos, block_count)?;
        let local_leaves = tree_size - leaf_offset as usize;
        if total_leaves != local_leaves {
            return Err(DbError::SnapshotCorrupted {
                reason: format!(
                    "block records sum to {total_leaves} but expected {local_leaves} local leaves"
                ),
            });
        }

        let leaves = read_leaves(payload, &mut pos, local_leaves)?;

        let mut db = DecryptionDb::with_offset(leaf_offset);
        db.leaves = leaves;
        db.blocks = blocks;
        Ok(db)
    }
}

fn read_u64(data: &[u8], pos: &mut usize) -> Result<u64, DbError> {
    if *pos + 8 > data.len() {
        return Err(DbError::SnapshotCorrupted {
            reason: "unexpected EOF reading u64".into(),
        });
    }
    let val = u64::from_le_bytes(data[*pos..*pos + 8].try_into().unwrap());
    *pos += 8;
    Ok(val)
}

fn read_block_records(
    data: &[u8],
    pos: &mut usize,
    count: usize,
) -> Result<(Vec<BlockRecord>, usize), DbError> {
    let end = *pos + count * BLOCK_RECORD_SIZE;
    if end > data.len() {
        return Err(DbError::SnapshotCorrupted {
            reason: "block records truncated".into(),
        });
    }

    let mut blocks = Vec::with_capacity(count);
    let mut total = 0usize;
    for _ in 0..count {
        let height = u64::from_le_bytes(data[*pos..*pos + 8].try_into().unwrap());
        *pos += 8;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&data[*pos..*pos + 32]);
        *pos += 32;
        let num_leaves = u32::from_le_bytes(data[*pos..*pos + 4].try_into().unwrap());
        *pos += 4;
        total += num_leaves as usize;
        blocks.push(BlockRecord {
            height,
            hash,
            num_leaves,
        });
    }
    Ok((blocks, total))
}

fn read_leaves(data: &[u8], pos: &mut usize, count: usize) -> Result<Vec<DecryptionLeaf>, DbError> {
    let end = *pos + count * DECRYPT_LEAF_BYTES;
    if end > data.len() {
        return Err(DbError::SnapshotCorrupted {
            reason: "leaf data truncated".into(),
        });
    }
    let mut leaves = Vec::with_capacity(count);
    for _ in 0..count {
        let leaf = DecryptionLeaf::from_bytes(&data[*pos..]).ok_or(DbError::SnapshotCorrupted {
            reason: "invalid leaf data".into(),
        })?;
        *pos += DECRYPT_LEAF_BYTES;
        leaves.push(leaf);
    }
    Ok(leaves)
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
    fn empty_snapshot_roundtrip() {
        let db = DecryptionDb::new();
        let snap = db.to_snapshot();
        let restored = DecryptionDb::from_snapshot(&snap).unwrap();

        assert_eq!(restored.tree_size(), 0);
        assert!(restored.blocks().is_empty());
        assert!(restored.leaves().is_empty());
    }

    #[test]
    fn populated_snapshot_roundtrip() {
        let mut db = DecryptionDb::new();
        db.append_leaves(100, [0xAA; 32], &[make_leaf(1), make_leaf(2)]);
        db.append_leaves(101, [0xBB; 32], &[make_leaf(3)]);

        let snap = db.to_snapshot();
        let restored = DecryptionDb::from_snapshot(&snap).unwrap();

        assert_eq!(restored.tree_size(), 3);
        assert_eq!(restored.blocks().len(), 2);
        assert_eq!(restored.latest_height(), Some(101));
        assert_eq!(restored.latest_block_hash(), Some([0xBB; 32]));
        assert_eq!(restored.leaves(), db.leaves());
    }

    #[test]
    fn snapshot_detects_corruption() {
        let mut db = DecryptionDb::new();
        db.append_leaves(100, [1u8; 32], &[make_leaf(1)]);

        let mut snap = db.to_snapshot();
        let mid = snap.len() / 2;
        snap[mid] ^= 0xFF;

        assert!(DecryptionDb::from_snapshot(&snap).is_err());
    }

    #[test]
    fn snapshot_detects_truncation() {
        assert!(DecryptionDb::from_snapshot(&[0u8; 10]).is_err());
    }

    #[test]
    fn snapshot_preserves_rollback_ability() {
        let mut db = DecryptionDb::new();
        db.append_leaves(100, [1u8; 32], &[make_leaf(1), make_leaf(2)]);
        db.append_leaves(101, [2u8; 32], &[make_leaf(3)]);

        let snap = db.to_snapshot();
        let mut restored = DecryptionDb::from_snapshot(&snap).unwrap();

        restored.rollback_to(100);
        assert_eq!(restored.tree_size(), 2);
        assert_eq!(restored.latest_height(), Some(100));
        assert_eq!(restored.leaves().len(), 2);
    }

    #[test]
    fn windowed_snapshot_roundtrip() {
        let offset = 3 * decryption_types::SHARD_LEAVES as u64;
        let mut db = DecryptionDb::with_offset(offset);
        db.append_leaves(200, [0xDD; 32], &[make_leaf(1), make_leaf(2)]);

        let snap = db.to_snapshot();
        let restored = DecryptionDb::from_snapshot(&snap).unwrap();

        assert_eq!(restored.tree_size(), db.tree_size());
        assert_eq!(restored.leaf_offset(), offset);
        assert_eq!(restored.leaves().len(), 2);
        assert_eq!(restored.latest_height(), Some(200));
        assert_eq!(restored.window_start_shard(), 3);
    }

    #[test]
    fn snapshot_rejects_tree_size_less_than_offset() {
        // Build a snapshot where tree_size < leaf_offset but with a valid
        // checksum so the structural validation (not the checksum) catches it.
        let offset = 3 * decryption_types::SHARD_LEAVES as u64;
        let bad_tree_size = 1u64; // less than offset
        let block_count = 0u64;
        let latest_height = 0u64;
        let latest_hash = [0u8; 32];

        let mut payload = Vec::new();
        payload.extend_from_slice(&SNAPSHOT_MAGIC_V1.to_le_bytes());
        payload.extend_from_slice(&bad_tree_size.to_le_bytes());
        payload.extend_from_slice(&block_count.to_le_bytes());
        payload.extend_from_slice(&latest_height.to_le_bytes());
        payload.extend_from_slice(&latest_hash);
        payload.extend_from_slice(&offset.to_le_bytes());

        let checksum = xxh64(&payload, 0);
        payload.extend_from_slice(&checksum.to_le_bytes());

        let result = DecryptionDb::from_snapshot(&payload);
        assert!(result.is_err());
        let msg = format!("{}", result.err().unwrap());
        assert!(
            msg.contains("tree_size"),
            "error should mention tree_size: {msg}"
        );
    }
}
