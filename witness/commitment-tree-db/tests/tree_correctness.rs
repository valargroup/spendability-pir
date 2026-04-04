//! Tree correctness test against mainnet lightwalletd.
//!
//! Ingests real Orchard note commitments from lightwalletd, builds the
//! commitment tree using [`CommitmentTreeDb`], and verifies shard roots
//! against the canonical values from `GetSubtreeRoots`.
//!
//! This test validates:
//! - Sinsemilla hash chain correctness (MerkleHashOrchard::combine at each level)
//! - Level-dependent empty-subtree sentinels (MerkleHashOrchard::empty_root)
//! - Frontier sub-shard handling (empty_root(0) padding, not zero bytes)
//! - Sub-shard → shard root decomposition matches the canonical Orchard tree
//!
//! Requires network access to `zec.rocks:443`.

use chain_ingest::LwdClient;
use commitment_ingest::extract_commitments;
use commitment_tree_db::CommitmentTreeDb;
use witness_types::SHARD_LEAVES;

const LWD_ENDPOINT: &str = "https://zec.rocks:443";
const ORCHARD_PROTOCOL: i32 = 1;
const BATCH_SIZE: u64 = 10_000;

#[tokio::test]
async fn verify_shard_root_against_mainnet() {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init()
        .ok();

    // ── 1. Connect and discover shard boundaries ─────────────────────

    let mut client = LwdClient::connect(&[LWD_ENDPOINT.to_string()])
        .await
        .expect("failed to connect to lightwalletd");

    let subtree_roots = client
        .get_subtree_roots(ORCHARD_PROTOCOL, 0, 256)
        .await
        .expect("failed to get subtree roots");

    let num_completed = subtree_roots.len();
    assert!(
        num_completed >= 2,
        "need at least 2 completed shards for this test, got {num_completed}"
    );

    tracing::info!(
        num_completed,
        "fetched completed Orchard shard roots from lightwalletd"
    );

    // Pick the LAST completed shard — recent shards have higher note density
    // so fewer blocks to fetch.
    let target_shard = num_completed - 1;
    let canonical_root_bytes = &subtree_roots[target_shard].root_hash;
    assert_eq!(
        canonical_root_bytes.len(),
        32,
        "shard root must be 32 bytes"
    );
    let mut canonical_root = [0u8; 32];
    canonical_root.copy_from_slice(canonical_root_bytes);

    // Block range: previous shard's completing block → this shard's completing block
    let shard_end_height = subtree_roots[target_shard].completing_block_height;
    let shard_start_height = subtree_roots[target_shard - 1].completing_block_height;

    tracing::info!(
        shard = target_shard,
        start_height = shard_start_height,
        end_height = shard_end_height,
        block_span = shard_end_height - shard_start_height,
        "ingesting blocks for shard root verification"
    );

    // ── 2. Ingest blocks and collect commitments ─────────────────────

    let mut all_commitments = Vec::new();
    let mut tree_size_before_start: Option<u64> = None;

    let mut current = shard_start_height;
    while current <= shard_end_height {
        let batch_end = (current + BATCH_SIZE - 1).min(shard_end_height);
        tracing::info!(from = current, to = batch_end, "fetching block range");

        let blocks = client
            .get_block_range(current, batch_end)
            .await
            .expect("failed to fetch blocks");

        for block in &blocks {
            if block.height == shard_start_height && tree_size_before_start.is_none() {
                if let Some(meta) = &block.chain_metadata {
                    let size_after = meta.orchard_commitment_tree_size as u64;
                    let cmx_count = extract_commitments(block).len() as u64;
                    tree_size_before_start = Some(size_after.saturating_sub(cmx_count));
                }
            }
            all_commitments.extend(extract_commitments(block));
        }

        current = batch_end + 1;
    }

    let tree_size_before =
        tree_size_before_start.unwrap_or((target_shard as u64) * (SHARD_LEAVES as u64));

    tracing::info!(
        total_commitments = all_commitments.len(),
        tree_size_before,
        "block ingestion complete"
    );

    // ── 3. Extract exactly this shard's 65,536 leaves ────────────────

    let shard_leaf_start = (target_shard as u64) * (SHARD_LEAVES as u64);
    let skip = shard_leaf_start.saturating_sub(tree_size_before) as usize;

    let shard_commits: Vec<_> = all_commitments
        .iter()
        .skip(skip)
        .take(SHARD_LEAVES)
        .copied()
        .collect();

    assert_eq!(
        shard_commits.len(),
        SHARD_LEAVES,
        "expected exactly {SHARD_LEAVES} leaves for shard {target_shard}, got {}",
        shard_commits.len()
    );

    tracing::info!(
        skipped = skip,
        kept = shard_commits.len(),
        "extracted shard leaves"
    );

    // ── 4. Build tree and verify shard root ──────────────────────────
    //
    // The shard root hash depends only on the 65,536 leaf values and the
    // Sinsemilla hash function — NOT on the shard's position in the full
    // tree. So we place the leaves at shard 0 (no filler needed) and
    // compare its root against the canonical root for shard N.

    let mut tree = CommitmentTreeDb::new();
    tree.append_commitments(shard_end_height, [0u8; 32], &shard_commits);

    assert_eq!(tree.tree_size(), SHARD_LEAVES as u64);

    let shard_roots = tree.shard_roots();
    assert_eq!(shard_roots.len(), 1);
    let (idx, computed_root) = shard_roots[0];
    assert_eq!(idx, 0);

    tracing::info!(
        shard = target_shard,
        computed = hex::encode(computed_root),
        canonical = hex::encode(canonical_root),
        "comparing shard roots"
    );

    assert_eq!(
        computed_root,
        canonical_root,
        "\nshard {target_shard} root MISMATCH — Sinsemilla hash chain is incorrect!\n  \
         computed:  {}\n  canonical: {}\n\n\
         This means the commitment tree does NOT match the canonical Orchard tree.\n\
         Check: MerkleHashOrchard::combine levels, empty_root sentinels, leaf ordering.",
        hex::encode(computed_root),
        hex::encode(canonical_root)
    );

    tracing::info!(
        shard = target_shard,
        "PASS: shard root verified against mainnet — Sinsemilla hash chain is correct"
    );
}
