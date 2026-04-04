//! Sync and follow loops for the witness ingest pipeline.
//!
//! Mirrors the nullifier ingest (`nf-ingest/ingest.rs`) but emits
//! [`WitnessChainEvent`] with note commitments instead of nullifiers.
//! Uses [`LwdClient`] and [`ChainTracker`] from `chain-ingest`.

use crate::parser::{extract_commitments, orchard_tree_size};
use chain_ingest::{ChainAction, ChainTracker, LwdClient};
use pir_types::CONFIRMATION_DEPTH;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};
use witness_types::WitnessChainEvent;

#[derive(Error, Debug)]
pub enum IngestError {
    #[error("client error: {0}")]
    Client(#[from] chain_ingest::ClientError),
}

pub type Result<T> = std::result::Result<T, IngestError>;

const SYNC_BATCH_SIZE: u64 = 10_000;
const FOLLOW_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Bulk-fetch blocks from `from` to `to` (inclusive) and emit WitnessChainEvents.
/// No reorg detection — used during initial sync catch-up.
///
/// Tracks `orchardCommitmentTreeSize` from each block's metadata to populate
/// `prior_tree_size` for the following block. The first block in each batch
/// uses `initial_tree_size` if provided.
pub async fn sync(
    client: &mut LwdClient,
    from: u64,
    to: u64,
    initial_tree_size: Option<u32>,
    tx: &mpsc::Sender<WitnessChainEvent>,
) -> Result<()> {
    let mut current = from;
    let mut prev_tree_size = initial_tree_size;

    while current <= to {
        let batch_end = (current + SYNC_BATCH_SIZE - 1).min(to);
        tracing::info!(
            from = current,
            to = batch_end,
            "fetching commitment block range"
        );

        let blocks = client.get_block_range(current, batch_end).await?;

        for block in &blocks {
            let height = block.height;
            let hash = to_hash_array(&block.hash);
            let prev_hash = to_hash_array(&block.prev_hash);
            let commitments = extract_commitments(block);
            let this_tree_size = orchard_tree_size(block);

            tx.send(WitnessChainEvent::NewBlock {
                height,
                hash,
                prev_hash,
                commitments,
                prior_tree_size: prev_tree_size,
            })
            .await
            .ok();

            prev_tree_size = this_tree_size;
        }

        current = batch_end + 1;
    }

    Ok(())
}

/// Follow the chain tip, emitting WitnessChainEvents for new blocks and reorgs.
/// Polls for new blocks at a fixed interval.
pub async fn follow(
    client: &mut LwdClient,
    start_height: u64,
    start_hash: [u8; 32],
    initial_tree_size: Option<u32>,
    tx: &mpsc::Sender<WitnessChainEvent>,
) -> Result<()> {
    let mut tracker =
        ChainTracker::with_tip(start_height, start_hash, CONFIRMATION_DEPTH as usize * 2);
    let mut current_height = start_height;
    let mut prev_tree_size = initial_tree_size;

    loop {
        let (tip_height, _tip_hash) = client.get_latest_block().await?;

        if tip_height <= current_height {
            sleep(FOLLOW_POLL_INTERVAL).await;
            continue;
        }

        let blocks = client
            .get_block_range(current_height + 1, tip_height)
            .await?;

        for block in &blocks {
            let height = block.height;
            let hash = to_hash_array(&block.hash);
            let prev_hash = to_hash_array(&block.prev_hash);
            let commitments = extract_commitments(block);
            let this_tree_size = orchard_tree_size(block);

            match tracker.push_block(height, hash, prev_hash) {
                ChainAction::Extend => {
                    tx.send(WitnessChainEvent::NewBlock {
                        height,
                        hash,
                        prev_hash,
                        commitments,
                        prior_tree_size: prev_tree_size,
                    })
                    .await
                    .ok();
                    current_height = height;
                }
                ChainAction::Reorg { rollback_to } => {
                    tx.send(WitnessChainEvent::Reorg { rollback_to }).await.ok();

                    tx.send(WitnessChainEvent::NewBlock {
                        height,
                        hash,
                        prev_hash,
                        commitments,
                        prior_tree_size: None,
                    })
                    .await
                    .ok();
                    current_height = height;
                }
            }

            prev_tree_size = this_tree_size;
        }

        sleep(FOLLOW_POLL_INTERVAL).await;
    }
}

fn to_hash_array(bytes: &[u8]) -> [u8; 32] {
    let mut arr = [0u8; 32];
    let len = bytes.len().min(32);
    arr[..len].copy_from_slice(&bytes[..len]);
    arr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_hash_array_pads_short() {
        let short = vec![1u8, 2, 3];
        let result = to_hash_array(&short);
        assert_eq!(result[0], 1);
        assert_eq!(result[1], 2);
        assert_eq!(result[2], 3);
        assert_eq!(result[3..], [0u8; 29]);
    }

    #[test]
    fn to_hash_array_truncates_long() {
        let long = vec![0xFFu8; 64];
        let result = to_hash_array(&long);
        assert_eq!(result, [0xFF; 32]);
    }

    #[test]
    fn to_hash_array_exact() {
        let exact = vec![42u8; 32];
        let result = to_hash_array(&exact);
        assert_eq!(result, [42u8; 32]);
    }
}
