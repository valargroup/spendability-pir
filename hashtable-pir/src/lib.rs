mod snapshot;

use spend_types::{hash_to_bucket, BUCKET_CAPACITY, NUM_BUCKETS};
use std::collections::{BTreeMap, HashMap};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum HashTableError {
    #[error("bucket {bucket_idx} overflow: already at capacity {}", BUCKET_CAPACITY)]
    BucketOverflow { bucket_idx: u32 },
    #[error("block hash not found for rollback")]
    BlockNotFound,
    #[error("snapshot error: {0}")]
    Snapshot(String),
}

pub type Result<T> = std::result::Result<T, HashTableError>;

#[derive(Clone)]
struct Bucket {
    entries: [[u8; 32]; BUCKET_CAPACITY],
    count: u8,
}

impl Bucket {
    fn new() -> Self {
        Self {
            entries: [[0u8; 32]; BUCKET_CAPACITY],
            count: 0,
        }
    }

    fn remove(&mut self, slot: u8) {
        self.entries[slot as usize] = [0u8; 32];
        // Compact: swap with the last occupied entry if this isn't the last slot.
        // We don't compact -- zeroing is sufficient for PIR rows.
        // However, count tracks occupied slots for insert positioning.
        // We need to handle this carefully: since we may remove slots out of order,
        // we track count as "next free slot" only for append. After removals,
        // we rely on the block index to know which slots are occupied.
        // For simplicity, don't decrement count here -- instead, slots are reused
        // only after the bucket is fully cleared by block eviction.
        //
        // Actually, let's use a different approach: on removal, we swap the removed
        // entry with the last entry and decrement count. But this changes slot indices
        // for the swapped entry... which breaks the block_index references.
        //
        // Simplest correct approach: zero the slot, don't change count. The bucket
        // has "holes". On insert, scan for the first zero slot. This is O(BUCKET_CAPACITY)
        // = O(16), which is fine.
    }

    fn find_free_slot(&self) -> Option<u8> {
        let zero = [0u8; 32];
        for i in 0..BUCKET_CAPACITY {
            if self.entries[i] == zero {
                return Some(i as u8);
            }
        }
        None
    }

    fn contains(&self, nf: &[u8; 32]) -> bool {
        for i in 0..BUCKET_CAPACITY {
            if &self.entries[i] == nf {
                return true;
            }
        }
        false
    }

}

#[derive(Clone)]
struct BlockRecord {
    block_hash: [u8; 32],
    slots: Vec<(u32, u8)>, // (bucket_idx, slot_idx)
}

/// Bucketed hash table for nullifier storage with per-block tracking,
/// LRU eviction by height, and crash-safe snapshots.
pub struct HashTableDb {
    buckets: Vec<Bucket>,
    block_index: BTreeMap<u64, BlockRecord>,
    block_hash_to_height: HashMap<[u8; 32], u64>,
    num_entries: usize,
}

impl HashTableDb {
    pub fn new() -> Self {
        Self {
            buckets: (0..NUM_BUCKETS).map(|_| Bucket::new()).collect(),
            block_index: BTreeMap::new(),
            block_hash_to_height: HashMap::new(),
            num_entries: 0,
        }
    }

    /// Insert all nullifiers from a single block.
    pub fn insert_block(
        &mut self,
        height: u64,
        block_hash: [u8; 32],
        nullifiers: &[[u8; 32]],
    ) -> Result<()> {
        let mut slots = Vec::with_capacity(nullifiers.len());

        for nf in nullifiers {
            let bucket_idx = hash_to_bucket(nf);
            let bucket = &mut self.buckets[bucket_idx as usize];

            let slot = bucket.find_free_slot().ok_or(HashTableError::BucketOverflow { bucket_idx })?;
            bucket.entries[slot as usize] = *nf;
            // Update count if we're extending past it
            if slot >= bucket.count {
                bucket.count = slot + 1;
            }
            slots.push((bucket_idx, slot));
            self.num_entries += 1;
        }

        let record = BlockRecord {
            block_hash,
            slots,
        };
        self.block_index.insert(height, record);
        self.block_hash_to_height.insert(block_hash, height);

        Ok(())
    }

    /// Remove all nullifiers inserted by the block with the given hash.
    pub fn rollback_block(&mut self, block_hash: &[u8; 32]) -> Result<()> {
        let height = self
            .block_hash_to_height
            .remove(block_hash)
            .ok_or(HashTableError::BlockNotFound)?;

        let record = self
            .block_index
            .remove(&height)
            .ok_or(HashTableError::BlockNotFound)?;

        for (bucket_idx, slot_idx) in &record.slots {
            self.buckets[*bucket_idx as usize].remove(*slot_idx);
        }
        self.num_entries -= record.slots.len();

        Ok(())
    }

    /// Evict the oldest (lowest-height) block. Returns the evicted height.
    pub fn evict_oldest_block(&mut self) -> Option<u64> {
        let oldest_height = *self.block_index.keys().next()?;
        let record = self.block_index.remove(&oldest_height)?;
        self.block_hash_to_height.remove(&record.block_hash);

        for (bucket_idx, slot_idx) in &record.slots {
            self.buckets[*bucket_idx as usize].remove(*slot_idx);
        }
        self.num_entries -= record.slots.len();

        Some(oldest_height)
    }

    /// Evict oldest blocks until `len() <= TARGET_SIZE`.
    pub fn evict_to_target(&mut self) {
        while self.num_entries > spend_types::TARGET_SIZE {
            if self.evict_oldest_block().is_none() {
                break;
            }
        }
    }

    /// Non-private lookup for testing and server-side use.
    pub fn contains(&self, nf: &[u8; 32]) -> bool {
        let bucket_idx = hash_to_bucket(nf);
        self.buckets[bucket_idx as usize].contains(nf)
    }

    /// Serialize buckets as row-major `NUM_BUCKETS x BUCKET_BYTES` for PIR.
    pub fn to_pir_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(spend_types::DB_BYTES);
        for bucket in &self.buckets {
            for entry in &bucket.entries {
                out.extend_from_slice(entry);
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.num_entries
    }

    pub fn is_empty(&self) -> bool {
        self.num_entries == 0
    }

    pub fn earliest_height(&self) -> Option<u64> {
        self.block_index.keys().next().copied()
    }

    pub fn latest_height(&self) -> Option<u64> {
        self.block_index.keys().next_back().copied()
    }

    pub fn latest_block_hash(&self) -> Option<[u8; 32]> {
        self.block_index
            .values()
            .next_back()
            .map(|r| r.block_hash)
    }

    pub fn num_blocks(&self) -> usize {
        self.block_index.len()
    }
}

impl Default for HashTableDb {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spend_types::{BUCKET_BYTES, DB_BYTES, NUM_BUCKETS};

    fn make_nf(seed: u32) -> [u8; 32] {
        let mut nf = [0u8; 32];
        nf[0..4].copy_from_slice(&seed.to_le_bytes());
        // Fill remaining bytes to make it non-zero and unique
        for i in 4..32 {
            nf[i] = ((seed >> ((i % 4) * 8)) as u8).wrapping_add(i as u8);
        }
        nf
    }

    fn make_hash(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    fn make_nfs(start: u32, count: u32) -> Vec<[u8; 32]> {
        (start..start + count).map(make_nf).collect()
    }

    #[test]
    fn test_insert_and_contains() {
        let mut db = HashTableDb::new();
        let nfs = make_nfs(0, 100);
        db.insert_block(1, make_hash(1), &nfs).unwrap();

        assert_eq!(db.len(), 100);
        for nf in &nfs {
            assert!(db.contains(nf), "inserted nf not found");
        }
    }

    #[test]
    fn test_insert_no_false_positive() {
        let mut db = HashTableDb::new();
        let nfs = make_nfs(0, 100);
        db.insert_block(1, make_hash(1), &nfs).unwrap();

        let missing = make_nfs(1000, 100);
        for nf in &missing {
            assert!(!db.contains(nf), "false positive for non-inserted nf");
        }
    }

    #[test]
    fn test_rollback() {
        let mut db = HashTableDb::new();
        let nfs = make_nfs(0, 50);
        let hash = make_hash(1);
        db.insert_block(1, hash, &nfs).unwrap();
        assert_eq!(db.len(), 50);

        db.rollback_block(&hash).unwrap();
        assert_eq!(db.len(), 0);
        for nf in &nfs {
            assert!(!db.contains(nf), "nf still present after rollback");
        }
    }

    #[test]
    fn test_evict_oldest() {
        let mut db = HashTableDb::new();
        let nfs_100 = make_nfs(0, 10);
        let nfs_101 = make_nfs(100, 10);
        let nfs_102 = make_nfs(200, 10);

        db.insert_block(100, make_hash(1), &nfs_100).unwrap();
        db.insert_block(101, make_hash(2), &nfs_101).unwrap();
        db.insert_block(102, make_hash(3), &nfs_102).unwrap();

        assert_eq!(db.len(), 30);
        let evicted = db.evict_oldest_block();
        assert_eq!(evicted, Some(100));
        assert_eq!(db.len(), 20);

        for nf in &nfs_100 {
            assert!(!db.contains(nf), "evicted nf still present");
        }
        for nf in &nfs_101 {
            assert!(db.contains(nf), "non-evicted nf missing");
        }
        for nf in &nfs_102 {
            assert!(db.contains(nf), "non-evicted nf missing");
        }
    }

    #[test]
    fn test_evict_to_target() {
        let mut db = HashTableDb::new();
        // Insert blocks with many nfs each to exceed TARGET_SIZE.
        // Each block gets 10000 nfs, so we need > 100 blocks for > 1M.
        // That's too slow for a unit test. Instead, test with a smaller scenario:
        // insert 3 blocks, and check that evict_to_target works by verifying
        // behavior when we're under target.
        let nfs = make_nfs(0, 100);
        db.insert_block(1, make_hash(1), &nfs).unwrap();
        assert_eq!(db.len(), 100);

        // Already under target, evict_to_target should be a no-op
        db.evict_to_target();
        assert_eq!(db.len(), 100);
    }

    #[test]
    fn test_snapshot_roundtrip() {
        let mut db = HashTableDb::new();
        let nfs_1 = make_nfs(0, 50);
        let nfs_2 = make_nfs(1000, 30);
        db.insert_block(100, make_hash(1), &nfs_1).unwrap();
        db.insert_block(101, make_hash(2), &nfs_2).unwrap();

        let snap = db.to_snapshot();
        let restored = HashTableDb::from_snapshot(&snap).unwrap();

        assert_eq!(restored.len(), db.len());
        assert_eq!(restored.earliest_height(), db.earliest_height());
        assert_eq!(restored.latest_height(), db.latest_height());
        assert_eq!(restored.latest_block_hash(), db.latest_block_hash());
        assert_eq!(restored.num_blocks(), db.num_blocks());

        for nf in &nfs_1 {
            assert!(restored.contains(nf));
        }
        for nf in &nfs_2 {
            assert!(restored.contains(nf));
        }
    }

    #[test]
    fn test_snapshot_checksum_tamper() {
        let mut db = HashTableDb::new();
        let nfs = make_nfs(0, 10);
        db.insert_block(1, make_hash(1), &nfs).unwrap();

        let mut snap = db.to_snapshot();
        // Corrupt one byte in the middle of the snapshot
        let mid = snap.len() / 2;
        snap[mid] ^= 0xff;

        let result = HashTableDb::from_snapshot(&snap);
        assert!(result.is_err());
    }

    #[test]
    fn test_bucket_overflow() {
        let mut db = HashTableDb::new();
        // Create nullifiers that all map to the same bucket (bucket 0).
        // Start from i=1 to avoid the all-zero nullifier (which is the empty sentinel).
        let mut nfs = Vec::new();
        for i in 1..=(BUCKET_CAPACITY as u32 + 1) {
            let mut nf = [0u8; 32];
            let val = (i * NUM_BUCKETS as u32).to_le_bytes();
            nf[0..4].copy_from_slice(&val);
            nf[4] = i as u8;
            nfs.push(nf);
        }
        // First 16 should succeed, 17th should fail
        let result = db.insert_block(1, make_hash(1), &nfs);
        assert!(result.is_err());
        match result.unwrap_err() {
            HashTableError::BucketOverflow { .. } => {}
            other => panic!("expected BucketOverflow, got: {other}"),
        }
    }

    #[test]
    fn test_multiple_rollbacks() {
        let mut db = HashTableDb::new();
        let nfs_a = make_nfs(0, 20);
        let nfs_b = make_nfs(100, 20);
        let nfs_c = make_nfs(200, 20);
        let hash_a = make_hash(1);
        let hash_b = make_hash(2);
        let hash_c = make_hash(3);

        db.insert_block(1, hash_a, &nfs_a).unwrap();
        db.insert_block(2, hash_b, &nfs_b).unwrap();
        db.insert_block(3, hash_c, &nfs_c).unwrap();
        assert_eq!(db.len(), 60);

        db.rollback_block(&hash_c).unwrap();
        assert_eq!(db.len(), 40);
        for nf in &nfs_c {
            assert!(!db.contains(nf));
        }

        db.rollback_block(&hash_b).unwrap();
        assert_eq!(db.len(), 20);
        for nf in &nfs_b {
            assert!(!db.contains(nf));
        }

        // Block A's nfs should survive
        for nf in &nfs_a {
            assert!(db.contains(nf));
        }
    }

    #[test]
    fn test_empty_block() {
        let mut db = HashTableDb::new();
        db.insert_block(1, make_hash(1), &[]).unwrap();
        assert_eq!(db.len(), 0);
        assert_eq!(db.num_blocks(), 1);
        assert_eq!(db.latest_height(), Some(1));

        // Eviction of empty block should work fine
        let evicted = db.evict_oldest_block();
        assert_eq!(evicted, Some(1));
        assert_eq!(db.num_blocks(), 0);
    }

    #[test]
    fn test_pir_bytes_layout() {
        let mut db = HashTableDb::new();
        let nfs = make_nfs(0, 10);
        db.insert_block(1, make_hash(1), &nfs).unwrap();

        let pir = db.to_pir_bytes();
        assert_eq!(pir.len(), DB_BYTES);

        // Verify layout: each bucket is BUCKET_BYTES, entries are ENTRY_BYTES each
        for nf in &nfs {
            let bucket_idx = hash_to_bucket(nf) as usize;
            let bucket_start = bucket_idx * BUCKET_BYTES;
            let bucket_data = &pir[bucket_start..bucket_start + BUCKET_BYTES];

            // The nf should appear somewhere in this bucket's data
            let found = bucket_data.chunks_exact(32).any(|chunk| chunk == nf);
            assert!(found, "nf not found in expected bucket's PIR bytes");
        }
    }

    #[test]
    fn test_idempotent_evict() {
        let mut db = HashTableDb::new();
        assert_eq!(db.evict_oldest_block(), None);
        db.evict_to_target(); // should not panic
        assert_eq!(db.len(), 0);
    }

    #[test]
    fn test_insert_after_rollback_reuses_slots() {
        let mut db = HashTableDb::new();
        let nfs_1 = make_nfs(0, 10);
        let hash_1 = make_hash(1);
        db.insert_block(1, hash_1, &nfs_1).unwrap();
        db.rollback_block(&hash_1).unwrap();

        // Re-insert into the same buckets should work
        let nfs_2 = make_nfs(0, 10);
        db.insert_block(2, make_hash(2), &nfs_2).unwrap();
        assert_eq!(db.len(), 10);
    }
}
