use chain_ingest::proto::CompactBlock;

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

#[cfg(test)]
mod tests {
    use super::*;
    use chain_ingest::proto::{CompactOrchardAction, CompactSaplingSpend, CompactTx};

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
}
