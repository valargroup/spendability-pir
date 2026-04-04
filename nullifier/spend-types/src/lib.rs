use serde::{Deserialize, Serialize};

// Re-export shared PIR types so existing consumers (spend-server, nf-ingest)
// continue to compile without import changes.
pub use pir_types::{
    PirEngine, ServerPhase, YpirScenario, CONFIRMATION_DEPTH, NU5_MAINNET_ACTIVATION,
};

pub const TARGET_SIZE: usize = 1_000_000;
pub const NUM_BUCKETS: usize = 16_384; // 2^14
pub const BUCKET_CAPACITY: usize = 112;
pub const ENTRY_BYTES: usize = 32;
pub const BUCKET_BYTES: usize = BUCKET_CAPACITY * ENTRY_BYTES; // 3,584 (= 28,672 bits, SimplePIR minimum row)
pub const DB_BYTES: usize = NUM_BUCKETS * BUCKET_BYTES; // ~56 MB

/// Map a nullifier to its bucket index.
/// Nullifiers are cryptographically random, so the first 4 bytes give uniform distribution.
pub fn hash_to_bucket(nf: &[u8; 32]) -> u32 {
    let raw = u32::from_le_bytes([nf[0], nf[1], nf[2], nf[3]]);
    raw % (NUM_BUCKETS as u32)
}

#[derive(Debug, Clone)]
pub enum ChainEvent {
    NewBlock {
        height: u64,
        hash: [u8; 32],
        prev_hash: [u8; 32],
        nullifiers: Vec<[u8; 32]>,
    },
    Reorg {
        orphaned: Vec<OrphanedBlock>,
        new_blocks: Vec<NewBlock>,
    },
}

#[derive(Debug, Clone)]
pub struct OrphanedBlock {
    pub height: u64,
    pub hash: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct NewBlock {
    pub height: u64,
    pub hash: [u8; 32],
    pub prev_hash: [u8; 32],
    pub nullifiers: Vec<[u8; 32]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpendabilityMetadata {
    pub earliest_height: u64,
    pub latest_height: u64,
    pub num_nullifiers: u64,
    pub num_buckets: u64,
    pub phase: ServerPhase,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_to_bucket_in_range() {
        let nf = [0xffu8; 32];
        let bucket = hash_to_bucket(&nf);
        assert!(bucket < NUM_BUCKETS as u32);
    }

    #[test]
    fn test_hash_to_bucket_deterministic() {
        let nf = [42u8; 32];
        assert_eq!(hash_to_bucket(&nf), hash_to_bucket(&nf));
    }

    #[test]
    fn test_hash_to_bucket_distribution() {
        use std::collections::HashMap;
        let mut counts: HashMap<u32, usize> = HashMap::new();
        for i in 0u32..10_000 {
            let mut nf = [0u8; 32];
            nf[0..4].copy_from_slice(&i.to_le_bytes());
            let bucket = hash_to_bucket(&nf);
            *counts.entry(bucket).or_default() += 1;
        }
        let max_count = counts.values().max().copied().unwrap_or(0);
        assert!(max_count < 10, "max bucket count {max_count} is too high");
    }

    #[test]
    fn test_hash_to_bucket_different_inputs() {
        let nf_a = [1u8; 32];
        let nf_b = [2u8; 32];
        let a = hash_to_bucket(&nf_a);
        let b = hash_to_bucket(&nf_b);
        assert_ne!(a, b);
    }

    #[test]
    fn test_constants_consistency() {
        assert_eq!(BUCKET_BYTES, BUCKET_CAPACITY * ENTRY_BYTES);
        assert_eq!(DB_BYTES, NUM_BUCKETS * BUCKET_BYTES);
        assert!(NUM_BUCKETS.is_power_of_two());
    }
}
