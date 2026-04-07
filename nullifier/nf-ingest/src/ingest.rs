use crate::parser::{extract_nullifiers_with_meta, orchard_tree_size};
use chain_ingest::{ChainAction, ChainTracker, LwdClient};
use spend_types::{ChainEvent, NewBlock, OrphanedBlock, CONFIRMATION_DEPTH};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

#[derive(Error, Debug)]
pub enum IngestError {
    #[error("client error: {0}")]
    Client(#[from] chain_ingest::ClientError),
    #[error("block hash mismatch at height {height}")]
    HashMismatch { height: u64 },
}

pub type Result<T> = std::result::Result<T, IngestError>;

const SYNC_BATCH_SIZE: u64 = 10_000;
const FOLLOW_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Bulk-fetch blocks from `from` to `to` (inclusive) and emit ChainEvents.
/// No reorg detection — used during sync mode catch-up.
///
/// `initial_tree_size` should be the `orchardCommitmentTreeSize` from the
/// block immediately before `from`, if known. Pass `None` on first sync.
pub async fn sync(
    client: &mut LwdClient,
    from: u64,
    to: u64,
    initial_tree_size: Option<u32>,
    tx: &mpsc::Sender<ChainEvent>,
) -> Result<()> {
    let mut current = from;
    let mut prev_tree_size = initial_tree_size;

    while current <= to {
        let batch_end = (current + SYNC_BATCH_SIZE - 1).min(to);
        tracing::info!(from = current, to = batch_end, "fetching block range");

        let blocks = client.get_block_range(current, batch_end).await?;

        for block in &blocks {
            let height = block.height;
            let hash = to_hash_array(&block.hash);
            let prev_hash = to_hash_array(&block.prev_hash);
            let (nullifiers, this_tree_size) = extract_nullifiers_with_meta(block, prev_tree_size);

            tx.send(ChainEvent::NewBlock {
                height,
                hash,
                prev_hash,
                nullifiers,
            })
            .await
            .ok();

            prev_tree_size = this_tree_size;
        }

        current = batch_end + 1;
    }

    Ok(())
}

/// Follow the chain tip, emitting ChainEvents for new blocks and reorgs.
/// Polls for new blocks at a fixed interval.
///
/// `initial_tree_size` should be the `orchardCommitmentTreeSize` at
/// `start_height`, if known.
pub async fn follow(
    client: &mut LwdClient,
    start_height: u64,
    start_hash: [u8; 32],
    initial_tree_size: Option<u32>,
    tx: &mpsc::Sender<ChainEvent>,
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
            let (nullifiers, this_tree_size) = extract_nullifiers_with_meta(block, prev_tree_size);

            match tracker.push_block(height, hash, prev_hash) {
                ChainAction::Extend => {
                    tx.send(ChainEvent::NewBlock {
                        height,
                        hash,
                        prev_hash,
                        nullifiers,
                    })
                    .await
                    .ok();
                    current_height = height;
                }
                ChainAction::Reorg { rollback_to } => {
                    let mut orphaned = Vec::new();
                    for h in (rollback_to + 1..=current_height).rev() {
                        orphaned.push(OrphanedBlock {
                            height: h,
                            hash: [0u8; 32],
                        });
                    }

                    let new_block = NewBlock {
                        height,
                        hash,
                        prev_hash,
                        nullifiers,
                    };

                    tx.send(ChainEvent::Reorg {
                        orphaned,
                        new_blocks: vec![new_block],
                    })
                    .await
                    .ok();
                    current_height = height;
                    // After a reorg, tree size for the replacement block is
                    // unreliable — reset so the next block uses chain_metadata.
                    prev_tree_size = orchard_tree_size(block);
                    continue;
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
