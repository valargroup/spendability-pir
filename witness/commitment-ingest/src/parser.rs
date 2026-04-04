//! Orchard note commitment extraction from compact blocks.
//!
//! Mirrors the nullifier parser (`nf-ingest/parser.rs`) but extracts `cmx`
//! (note commitment) values instead of nullifiers from each `CompactOrchardAction`.

use chain_ingest::proto::CompactBlock;
use witness_types::Hash;

/// Extracted commitments from a single compact block.
#[derive(Debug, Clone)]
pub struct BlockCommitments {
    /// Block height.
    pub height: u64,
    /// Block hash.
    pub hash: [u8; 32],
    /// Previous block hash.
    pub prev_hash: [u8; 32],
    /// Orchard note commitments in transaction-then-action order.
    pub commitments: Vec<Hash>,
    /// `orchardCommitmentTreeSize` from the *previous* block's metadata,
    /// giving the tree size at the start of this block.
    pub prior_tree_size: Option<u32>,
}

/// Extract all Orchard note commitments (`cmx`) from a compact block.
///
/// Walks all transactions and collects the 32-byte `cmx` from each Orchard
/// action. Sapling outputs are ignored — the witness system tracks only Orchard.
pub fn extract_commitments(block: &CompactBlock) -> Vec<Hash> {
    let mut commitments = Vec::new();
    for tx in &block.vtx {
        for action in &tx.actions {
            if action.cmx.len() == 32 {
                let mut cmx = [0u8; 32];
                cmx.copy_from_slice(&action.cmx);
                commitments.push(cmx);
            }
        }
    }
    commitments
}

/// Extract full block commitment data including metadata.
///
/// `prev_tree_size` should be the `orchardCommitmentTreeSize` from the
/// previous block's `ChainMetadata`, if available. For the first block
/// in a batch, pass `None`.
pub fn extract_block_commitments(
    block: &CompactBlock,
    prev_tree_size: Option<u32>,
) -> BlockCommitments {
    let mut hash = [0u8; 32];
    let len = block.hash.len().min(32);
    hash[..len].copy_from_slice(&block.hash[..len]);

    let mut prev_hash = [0u8; 32];
    let len = block.prev_hash.len().min(32);
    prev_hash[..len].copy_from_slice(&block.prev_hash[..len]);

    BlockCommitments {
        height: block.height,
        hash,
        prev_hash,
        commitments: extract_commitments(block),
        prior_tree_size: prev_tree_size,
    }
}

/// Get the `orchardCommitmentTreeSize` from a block's chain metadata, if present.
pub fn orchard_tree_size(block: &CompactBlock) -> Option<u32> {
    block
        .chain_metadata
        .as_ref()
        .map(|m| m.orchard_commitment_tree_size)
        .filter(|&size| size > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chain_ingest::proto::{
        ChainMetadata, CompactOrchardAction, CompactSaplingOutput, CompactTx,
    };

    fn make_action(cmx_byte: u8, nf_byte: u8) -> CompactOrchardAction {
        CompactOrchardAction {
            nullifier: vec![nf_byte; 32],
            cmx: vec![cmx_byte; 32],
            ephemeral_key: vec![0; 32],
            ciphertext: vec![0; 52],
        }
    }

    fn make_sapling_output(cmu_byte: u8) -> CompactSaplingOutput {
        CompactSaplingOutput {
            cmu: vec![cmu_byte; 32],
            ephemeral_key: vec![0; 32],
            ciphertext: vec![0; 52],
        }
    }

    #[test]
    fn extract_basic_commitments() {
        let block = CompactBlock {
            height: 100,
            hash: vec![1; 32],
            prev_hash: vec![0; 32],
            vtx: vec![CompactTx {
                actions: vec![make_action(0xAA, 0x11), make_action(0xBB, 0x22)],
                ..Default::default()
            }],
            ..Default::default()
        };

        let cmxs = extract_commitments(&block);
        assert_eq!(cmxs.len(), 2);
        assert_eq!(cmxs[0], [0xAA; 32]);
        assert_eq!(cmxs[1], [0xBB; 32]);
    }

    #[test]
    fn extract_empty_block() {
        let block = CompactBlock {
            height: 100,
            vtx: vec![],
            ..Default::default()
        };

        let cmxs = extract_commitments(&block);
        assert!(cmxs.is_empty());
    }

    #[test]
    fn extract_ignores_sapling() {
        let block = CompactBlock {
            height: 100,
            vtx: vec![CompactTx {
                outputs: vec![make_sapling_output(0xCC)],
                actions: vec![make_action(0xDD, 0x33)],
                ..Default::default()
            }],
            ..Default::default()
        };

        let cmxs = extract_commitments(&block);
        assert_eq!(cmxs.len(), 1, "should only extract Orchard cmx");
        assert_eq!(cmxs[0], [0xDD; 32]);
    }

    #[test]
    fn extract_multiple_transactions() {
        let block = CompactBlock {
            height: 100,
            vtx: vec![
                CompactTx {
                    actions: vec![make_action(0x01, 0xA1)],
                    ..Default::default()
                },
                CompactTx {
                    actions: vec![make_action(0x02, 0xA2), make_action(0x03, 0xA3)],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let cmxs = extract_commitments(&block);
        assert_eq!(cmxs.len(), 3);
        assert_eq!(cmxs[0], [0x01; 32]);
        assert_eq!(cmxs[1], [0x02; 32]);
        assert_eq!(cmxs[2], [0x03; 32]);
    }

    #[test]
    fn extract_ignores_short_cmx() {
        let block = CompactBlock {
            height: 100,
            vtx: vec![CompactTx {
                actions: vec![CompactOrchardAction {
                    nullifier: vec![0; 32],
                    cmx: vec![0xFF; 16], // too short
                    ephemeral_key: vec![0; 32],
                    ciphertext: vec![0; 52],
                }],
                ..Default::default()
            }],
            ..Default::default()
        };

        let cmxs = extract_commitments(&block);
        assert!(cmxs.is_empty());
    }

    #[test]
    fn extract_block_commitments_with_metadata() {
        let block = CompactBlock {
            height: 200,
            hash: vec![0xAA; 32],
            prev_hash: vec![0xBB; 32],
            vtx: vec![CompactTx {
                actions: vec![make_action(0xCC, 0x44)],
                ..Default::default()
            }],
            chain_metadata: Some(ChainMetadata {
                sapling_commitment_tree_size: 0,
                orchard_commitment_tree_size: 1000,
            }),
            ..Default::default()
        };

        let bc = extract_block_commitments(&block, Some(999));
        assert_eq!(bc.height, 200);
        assert_eq!(bc.hash, [0xAA; 32]);
        assert_eq!(bc.prev_hash, [0xBB; 32]);
        assert_eq!(bc.commitments.len(), 1);
        assert_eq!(bc.prior_tree_size, Some(999));
    }

    #[test]
    fn orchard_tree_size_present() {
        let block = CompactBlock {
            chain_metadata: Some(ChainMetadata {
                sapling_commitment_tree_size: 0,
                orchard_commitment_tree_size: 5000,
            }),
            ..Default::default()
        };
        assert_eq!(orchard_tree_size(&block), Some(5000));
    }

    #[test]
    fn orchard_tree_size_absent() {
        let block = CompactBlock {
            chain_metadata: None,
            ..Default::default()
        };
        assert_eq!(orchard_tree_size(&block), None);
    }

    #[test]
    fn orchard_tree_size_zero_treated_as_absent() {
        let block = CompactBlock {
            chain_metadata: Some(ChainMetadata {
                sapling_commitment_tree_size: 0,
                orchard_commitment_tree_size: 0,
            }),
            ..Default::default()
        };
        assert_eq!(orchard_tree_size(&block), None);
    }

    #[test]
    fn commitment_ordering_matches_action_order() {
        let block = CompactBlock {
            height: 100,
            vtx: vec![
                CompactTx {
                    actions: vec![make_action(0x01, 0xA1), make_action(0x02, 0xA2)],
                    ..Default::default()
                },
                CompactTx {
                    actions: vec![make_action(0x03, 0xA3)],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let cmxs = extract_commitments(&block);
        assert_eq!(cmxs[0], [0x01; 32]);
        assert_eq!(cmxs[1], [0x02; 32]);
        assert_eq!(cmxs[2], [0x03; 32]);
    }
}
