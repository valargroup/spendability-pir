use chain_ingest::proto::CompactBlock;
use spend_types::NullifierWithMeta;

/// Extract all Orchard nullifiers from a compact block.
///
/// Iterates through all transactions and collects the 32-byte nullifier from
/// each Orchard action. Sapling nullifiers are ignored — the PIR database
/// tracks only Orchard.
pub fn extract_nullifiers(block: &CompactBlock) -> Vec<[u8; 32]> {
    let mut nullifiers = Vec::new();
    for tx in &block.vtx {
        for action in &tx.actions {
            if action.nullifier.len() == 32 {
                let mut nf = [0u8; 32];
                nf.copy_from_slice(&action.nullifier);
                nullifiers.push(nf);
            }
        }
    }
    nullifiers
}

/// Get the `orchardCommitmentTreeSize` from a block's chain metadata, if present.
pub fn orchard_tree_size(block: &CompactBlock) -> Option<u32> {
    block
        .chain_metadata
        .as_ref()
        .map(|m| m.orchard_commitment_tree_size)
        .filter(|&size| size > 0)
}

/// Extract Orchard nullifiers with per-transaction output position metadata.
///
/// For each nullifier, records the tree position of the first output in its
/// transaction and the transaction's action count. This lets clients address
/// the decryption/witness PIR databases without downloading the block.
///
/// Returns `(nullifiers_with_meta, this_block_tree_size)`. The second value
/// should be passed as `prior_tree_size` for the next block.
pub fn extract_nullifiers_with_meta(
    block: &CompactBlock,
    prior_tree_size: Option<u32>,
) -> (Vec<NullifierWithMeta>, Option<u32>) {
    let this_tree_size = orchard_tree_size(block);

    let total_actions: u32 = block.vtx.iter().map(|tx| tx.actions.len() as u32).sum();

    // Compute tree size at the start of this block:
    // preferred: derive from this block's metadata (end - total_actions)
    // fallback:  use prior block's metadata (prior_tree_size)
    let tree_size_before_block = this_tree_size
        .and_then(|ts| ts.checked_sub(total_actions))
        .or(prior_tree_size);

    let mut results = Vec::new();
    let mut running_position: Option<u32> = tree_size_before_block;

    for tx in &block.vtx {
        let action_count = tx.actions.len() as u8;
        let first_output_position = running_position.unwrap_or(0);

        for action in &tx.actions {
            if action.nullifier.len() == 32 {
                let mut nf = [0u8; 32];
                nf.copy_from_slice(&action.nullifier);
                results.push(NullifierWithMeta {
                    nullifier: nf,
                    first_output_position,
                    action_count,
                });
            }
        }

        if let Some(pos) = &mut running_position {
            *pos += tx.actions.len() as u32;
        }
    }

    (results, this_tree_size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chain_ingest::proto::{
        ChainMetadata, CompactOrchardAction, CompactSaplingSpend, CompactTx,
    };

    fn make_orchard_action(nf_byte: u8) -> CompactOrchardAction {
        CompactOrchardAction {
            nullifier: vec![nf_byte; 32],
            cmx: vec![0; 32],
            ephemeral_key: vec![0; 32],
            ciphertext: vec![0; 52],
        }
    }

    fn make_sapling_spend(nf_byte: u8) -> CompactSaplingSpend {
        CompactSaplingSpend {
            nf: vec![nf_byte; 32],
        }
    }

    #[test]
    fn test_extract_nullifiers() {
        let block = CompactBlock {
            height: 1,
            hash: vec![0; 32],
            prev_hash: vec![0; 32],
            vtx: vec![CompactTx {
                actions: vec![make_orchard_action(0xAA), make_orchard_action(0xBB)],
                ..Default::default()
            }],
            ..Default::default()
        };

        let nfs = extract_nullifiers(&block);
        assert_eq!(nfs.len(), 2);
        assert_eq!(nfs[0], [0xAA; 32]);
        assert_eq!(nfs[1], [0xBB; 32]);
    }

    #[test]
    fn test_extract_empty_block() {
        let block = CompactBlock {
            height: 1,
            hash: vec![0; 32],
            prev_hash: vec![0; 32],
            vtx: vec![],
            ..Default::default()
        };

        let nfs = extract_nullifiers(&block);
        assert!(nfs.is_empty());
    }

    #[test]
    fn test_extract_mixed_pools() {
        let block = CompactBlock {
            height: 1,
            hash: vec![0; 32],
            prev_hash: vec![0; 32],
            vtx: vec![CompactTx {
                spends: vec![make_sapling_spend(0xCC)],
                actions: vec![make_orchard_action(0xDD)],
                ..Default::default()
            }],
            ..Default::default()
        };

        let nfs = extract_nullifiers(&block);
        assert_eq!(nfs.len(), 1, "should only extract Orchard nullifiers");
        assert_eq!(nfs[0], [0xDD; 32]);
    }

    #[test]
    fn test_extract_multiple_txs() {
        let block = CompactBlock {
            height: 1,
            hash: vec![0; 32],
            prev_hash: vec![0; 32],
            vtx: vec![
                CompactTx {
                    actions: vec![make_orchard_action(0x01)],
                    ..Default::default()
                },
                CompactTx {
                    actions: vec![make_orchard_action(0x02), make_orchard_action(0x03)],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let nfs = extract_nullifiers(&block);
        assert_eq!(nfs.len(), 3);
    }

    #[test]
    fn test_extract_ignores_short_nullifier() {
        let block = CompactBlock {
            height: 1,
            vtx: vec![CompactTx {
                actions: vec![CompactOrchardAction {
                    nullifier: vec![0xFF; 16], // too short
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };

        let nfs = extract_nullifiers(&block);
        assert!(nfs.is_empty());
    }

    // --- extract_nullifiers_with_meta tests ---

    #[test]
    fn meta_single_tx_with_chain_metadata() {
        // Block has 2 actions, tree size ends at 1002 => started at 1000
        let block = CompactBlock {
            height: 100,
            hash: vec![0; 32],
            prev_hash: vec![0; 32],
            vtx: vec![CompactTx {
                actions: vec![make_orchard_action(0xAA), make_orchard_action(0xBB)],
                ..Default::default()
            }],
            chain_metadata: Some(ChainMetadata {
                sapling_commitment_tree_size: 0,
                orchard_commitment_tree_size: 1002,
            }),
            ..Default::default()
        };

        let (results, next_ts) = extract_nullifiers_with_meta(&block, None);
        assert_eq!(results.len(), 2);
        // Both nullifiers from the same tx share first_output_position and action_count
        assert_eq!(results[0].first_output_position, 1000);
        assert_eq!(results[0].action_count, 2);
        assert_eq!(results[1].first_output_position, 1000);
        assert_eq!(results[1].action_count, 2);
        assert_eq!(results[0].nullifier, [0xAA; 32]);
        assert_eq!(results[1].nullifier, [0xBB; 32]);
        assert_eq!(next_ts, Some(1002));
    }

    #[test]
    fn meta_multiple_txs_positions_increment() {
        // tx0: 1 action, tx1: 3 actions; tree ends at 5004 => started at 5000
        let block = CompactBlock {
            height: 200,
            vtx: vec![
                CompactTx {
                    actions: vec![make_orchard_action(0x01)],
                    ..Default::default()
                },
                CompactTx {
                    actions: vec![
                        make_orchard_action(0x02),
                        make_orchard_action(0x03),
                        make_orchard_action(0x04),
                    ],
                    ..Default::default()
                },
            ],
            chain_metadata: Some(ChainMetadata {
                sapling_commitment_tree_size: 0,
                orchard_commitment_tree_size: 5004,
            }),
            ..Default::default()
        };

        let (results, _) = extract_nullifiers_with_meta(&block, None);
        assert_eq!(results.len(), 4);
        // tx0 starts at position 5000, action_count 1
        assert_eq!(results[0].first_output_position, 5000);
        assert_eq!(results[0].action_count, 1);
        // tx1 starts at position 5001, action_count 3
        assert_eq!(results[1].first_output_position, 5001);
        assert_eq!(results[1].action_count, 3);
        assert_eq!(results[2].first_output_position, 5001);
        assert_eq!(results[3].first_output_position, 5001);
    }

    #[test]
    fn meta_fallback_to_prior_tree_size() {
        // No chain_metadata; use prior_tree_size as the starting position
        let block = CompactBlock {
            height: 300,
            vtx: vec![CompactTx {
                actions: vec![make_orchard_action(0xCC)],
                ..Default::default()
            }],
            chain_metadata: None,
            ..Default::default()
        };

        let (results, next_ts) = extract_nullifiers_with_meta(&block, Some(8000));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].first_output_position, 8000);
        assert_eq!(results[0].action_count, 1);
        // No chain_metadata => next tree size unknown
        assert_eq!(next_ts, None);
    }

    #[test]
    fn meta_no_metadata_at_all_degrades_to_zero() {
        let block = CompactBlock {
            height: 400,
            vtx: vec![CompactTx {
                actions: vec![make_orchard_action(0xDD)],
                ..Default::default()
            }],
            chain_metadata: None,
            ..Default::default()
        };

        let (results, _) = extract_nullifiers_with_meta(&block, None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].first_output_position, 0);
        assert_eq!(results[0].action_count, 1);
    }

    #[test]
    fn meta_empty_block() {
        let block = CompactBlock {
            height: 500,
            vtx: vec![],
            chain_metadata: Some(ChainMetadata {
                sapling_commitment_tree_size: 0,
                orchard_commitment_tree_size: 9000,
            }),
            ..Default::default()
        };

        let (results, next_ts) = extract_nullifiers_with_meta(&block, None);
        assert!(results.is_empty());
        assert_eq!(next_ts, Some(9000));
    }
}
