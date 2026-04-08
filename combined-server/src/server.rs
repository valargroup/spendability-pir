use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use chain_ingest::{ChainAction, ChainTracker, LwdClient};
use pir_types::{PirEngine, ServerPhase, CONFIRMATION_DEPTH};
use serde::Serialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

#[cfg(feature = "nullifier")]
use hashtable_pir::HashTableDb;
#[cfg(feature = "nullifier")]
use spend_types::{BUCKET_BYTES, NUM_BUCKETS};

#[cfg(feature = "witness")]
use commitment_tree_db::CommitmentTreeDb;
#[cfg(feature = "witness")]
use witness_types::{L0_DB_ROWS, SUBSHARD_ROW_BYTES};

#[cfg(feature = "decryption")]
use decryption_db::DecryptionDb;
#[cfg(feature = "decryption")]
use decryption_types::{DECRYPT_DB_ROWS, DECRYPT_ROW_BYTES};

const FOLLOW_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct CombinedConfig {
    #[cfg(feature = "nullifier")]
    pub target_size: usize,
    pub snapshot_interval: u64,
    pub data_dir: PathBuf,
    pub lwd_urls: Vec<String>,
    pub listen_addr: SocketAddr,
}

#[derive(thiserror::Error, Debug)]
pub enum ServerError {
    #[cfg(feature = "nullifier")]
    #[error("nullifier server error: {0}")]
    Nullifier(#[from] spend_server::server::ServerError),
    #[cfg(feature = "witness")]
    #[error("witness server error: {0}")]
    Witness(#[from] witness_server::server::ServerError),
    #[cfg(feature = "decryption")]
    #[error("decryption server error: {0}")]
    Decryption(#[from] decryption_server::server::ServerError),
    #[error("chain client error: {0}")]
    Client(#[from] chain_ingest::ClientError),
    #[cfg(feature = "nullifier")]
    #[error("hashtable error: {0}")]
    HashTable(#[from] hashtable_pir::HashTableError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ServerError>;

struct CombinedHealthState {
    #[cfg(feature = "nullifier")]
    nf_phase: Arc<arc_swap::ArcSwap<ServerPhase>>,
    #[cfg(feature = "witness")]
    wit_phase: Arc<arc_swap::ArcSwap<ServerPhase>>,
    #[cfg(feature = "decryption")]
    dec_phase: Arc<arc_swap::ArcSwap<ServerPhase>>,
}

#[derive(Serialize)]
struct CombinedHealthResponse {
    #[cfg(feature = "nullifier")]
    nullifier: SubsystemHealth,
    #[cfg(feature = "witness")]
    witness: SubsystemHealth,
    #[cfg(feature = "decryption")]
    decryption: SubsystemHealth,
}

#[derive(Serialize)]
struct SubsystemHealth {
    phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_height: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_height: Option<u64>,
}

fn phase_to_health(phase: &ServerPhase) -> SubsystemHealth {
    match phase {
        ServerPhase::Serving => SubsystemHealth {
            phase: "serving".into(),
            current_height: None,
            target_height: None,
        },
        ServerPhase::Syncing {
            current_height,
            target_height,
        } => SubsystemHealth {
            phase: "syncing".into(),
            current_height: Some(*current_height),
            target_height: Some(*target_height),
        },
    }
}

async fn combined_health(State(state): State<Arc<CombinedHealthState>>) -> impl IntoResponse {
    #[allow(unused_mut)]
    let mut all_serving = true;

    #[cfg(feature = "nullifier")]
    let nf_health = {
        let nf_phase = state.nf_phase.load();
        if !matches!(nf_phase.as_ref(), ServerPhase::Serving) {
            all_serving = false;
        }
        phase_to_health(&nf_phase)
    };

    #[cfg(feature = "witness")]
    let wit_health = {
        let wit_phase = state.wit_phase.load();
        if !matches!(wit_phase.as_ref(), ServerPhase::Serving) {
            all_serving = false;
        }
        phase_to_health(&wit_phase)
    };

    #[cfg(feature = "decryption")]
    let dec_health = {
        let dec_phase = state.dec_phase.load();
        if !matches!(dec_phase.as_ref(), ServerPhase::Serving) {
            all_serving = false;
        }
        phase_to_health(&dec_phase)
    };

    let body = CombinedHealthResponse {
        #[cfg(feature = "nullifier")]
        nullifier: nf_health,
        #[cfg(feature = "witness")]
        witness: wit_health,
        #[cfg(feature = "decryption")]
        decryption: dec_health,
    };

    let status = if all_serving {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (status, axum::Json(body))
}

/// Main entry point for the combined PIR server.
pub async fn run<
    #[cfg(feature = "nullifier")] NfP: PirEngine + 'static,
    #[cfg(feature = "witness")] WitP: PirEngine + 'static,
    #[cfg(feature = "decryption")] DecP: PirEngine + 'static,
>(
    config: CombinedConfig,
    #[cfg(feature = "nullifier")] nf_engine: Arc<NfP>,
    #[cfg(feature = "witness")] wit_engine: Arc<WitP>,
    #[cfg(feature = "decryption")] dec_engine: Arc<DecP>,
) -> Result<()> {
    // --- Sync phase ---

    #[cfg(feature = "nullifier")]
    let nf_config = {
        let nf_data_dir = config.data_dir.join("nullifier");
        spend_server::state::ServerConfig {
            target_size: config.target_size,
            confirmation_depth: CONFIRMATION_DEPTH,
            snapshot_interval: config.snapshot_interval,
            data_dir: nf_data_dir,
            lwd_urls: config.lwd_urls.clone(),
            listen_addr: config.listen_addr,
        }
    };

    #[cfg(feature = "witness")]
    let wit_config = {
        let wit_data_dir = config.data_dir.join("witness");
        witness_server::state::ServerConfig {
            snapshot_interval: config.snapshot_interval,
            data_dir: wit_data_dir,
            lwd_urls: config.lwd_urls.clone(),
            listen_addr: config.listen_addr,
            window_shard_limit: witness_server::state::DEFAULT_WINDOW_SHARD_LIMIT,
        }
    };

    tracing::info!("starting sync for enabled subsystems");

    #[cfg(all(feature = "nullifier", feature = "witness"))]
    let ((nf_state, mut hashtable), (wit_state, mut tree)) = {
        let nf_sync = spend_server::server::run_sync_only(nf_config.clone(), nf_engine.clone());
        let wit_sync =
            witness_server::server::run_sync_only(wit_config.clone(), wit_engine.clone());
        let (nf_result, wit_result) = tokio::join!(nf_sync, wit_sync);
        (
            nf_result.map_err(ServerError::Nullifier)?,
            wit_result.map_err(ServerError::Witness)?,
        )
    };

    #[cfg(all(feature = "nullifier", not(feature = "witness")))]
    let (nf_state, mut hashtable) = {
        spend_server::server::run_sync_only(nf_config.clone(), nf_engine.clone())
            .await
            .map_err(ServerError::Nullifier)?
    };

    #[cfg(all(feature = "witness", not(feature = "nullifier")))]
    let (wit_state, mut tree) = {
        witness_server::server::run_sync_only(wit_config.clone(), wit_engine.clone())
            .await
            .map_err(ServerError::Witness)?
    };

    // --- Decryption sync (runs after witness, shares its window) ---

    #[cfg(feature = "decryption")]
    let (dec_state, mut dec_db) = {
        let dec_data_dir = config.data_dir.join("decryption");
        std::fs::create_dir_all(&dec_data_dir)?;

        let mut dec_db = match decryption_server::snapshot_io::load_snapshot(&dec_data_dir).await {
            Ok(db) => {
                tracing::info!(
                    latest_height = ?db.latest_height(),
                    tree_size = db.tree_size(),
                    leaf_offset = db.leaf_offset(),
                    "loaded decryption snapshot"
                );
                db
            }
            Err(e) => {
                let offset = tree.leaf_offset();
                tracing::info!(
                    leaf_offset = offset,
                    error = %e,
                    "no decryption snapshot, creating fresh db"
                );
                DecryptionDb::with_offset(offset)
            }
        };

        let wit_latest = tree.latest_height().unwrap_or(0);

        let catch_up_from = match dec_db.latest_height() {
            Some(h) if h >= wit_latest => None,
            Some(h) => Some(h + 1),
            None => Some(
                decryption_sync_start(&config.lwd_urls, dec_db.leaf_offset()).await?,
            ),
        };
        if let Some(from) = catch_up_from {
            if from <= wit_latest {
                tracing::info!(from, to = wit_latest, "catching up decryption db");
                catch_up_decryption(&config.lwd_urls, from, wit_latest, &mut dec_db).await?;
            }
        }

        decryption_server::snapshot_io::save_snapshot(&dec_db, &dec_data_dir)
            .await
            .map_err(decryption_server::server::ServerError::from)?;

        let state = Arc::new(decryption_server::state::AppState::new(dec_engine.clone()));
        let anchor_height = dec_db.latest_height().unwrap_or(0);
        let pir = decryption_server::server::rebuild_pir(
            &*dec_engine,
            &dec_db,
            &state.scenario,
            anchor_height,
        )?;
        state.live_pir.store(Arc::new(Some(pir)));
        state.phase.store(Arc::new(ServerPhase::Serving));
        tracing::info!(anchor_height, tree_size = dec_db.tree_size(), "decryption pir ready");

        (state, dec_db)
    };

    tracing::info!("sync complete, building router");

    // --- Build router ---

    let health_state = Arc::new(CombinedHealthState {
        #[cfg(feature = "nullifier")]
        nf_phase: Arc::new(arc_swap::ArcSwap::from_pointee(ServerPhase::Serving)),
        #[cfg(feature = "witness")]
        wit_phase: Arc::new(arc_swap::ArcSwap::from_pointee(ServerPhase::Serving)),
        #[cfg(feature = "decryption")]
        dec_phase: Arc::new(arc_swap::ArcSwap::from_pointee(ServerPhase::Serving)),
    });

    #[allow(unused_mut)]
    let mut router = Router::new()
        .route("/health", get(combined_health))
        .with_state(health_state);

    #[cfg(feature = "nullifier")]
    {
        router = router.nest(
            "/nullifier",
            spend_server::server::build_router(nf_state.clone()),
        );
    }

    #[cfg(feature = "witness")]
    {
        router = router.nest(
            "/witness",
            witness_server::server::build_router(wit_state.clone()),
        );
    }

    #[cfg(feature = "decryption")]
    {
        router = router.nest(
            "/decrypt",
            decryption_server::server::build_router(dec_state.clone()),
        );
    }

    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!(listen = %config.listen_addr, "http server started");
    let _http_handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    // --- Follow loop ---

    // Determine the starting height/hash from whichever subsystem(s) are enabled.
    // When both are enabled, catch up the one that's behind first.
    #[cfg(all(feature = "nullifier", feature = "witness"))]
    let (follow_height, follow_hash) = {
        let nf_latest = hashtable.latest_height().unwrap_or(0);
        let wit_latest = tree.latest_height().unwrap_or(0);
        match nf_latest.cmp(&wit_latest) {
            std::cmp::Ordering::Less => {
                tracing::info!(
                    from = nf_latest + 1,
                    to = wit_latest,
                    "catching up nullifier subsystem"
                );
                catch_up_nullifier(&config.lwd_urls, nf_latest + 1, wit_latest, &mut hashtable)
                    .await?;
            }
            std::cmp::Ordering::Greater => {
                tracing::info!(from = wit_latest + 1, to = nf_latest, "catching up witness");
                let ts = if tree.tree_size() > 0 {
                    Some(tree.tree_size() as u32)
                } else {
                    None
                };
                catch_up_witness(&config.lwd_urls, wit_latest + 1, nf_latest, &mut tree, ts)
                    .await?;
            }
            std::cmp::Ordering::Equal => {}
        }
        let h = hashtable.latest_height().unwrap_or(0);
        let hash = hashtable.latest_block_hash().unwrap_or([0u8; 32]);
        (h, hash)
    };

    #[cfg(all(feature = "nullifier", not(feature = "witness")))]
    let (follow_height, follow_hash) = {
        let h = hashtable.latest_height().unwrap_or(0);
        let hash = hashtable.latest_block_hash().unwrap_or([0u8; 32]);
        (h, hash)
    };

    #[cfg(all(feature = "witness", not(feature = "nullifier")))]
    let (follow_height, follow_hash) = {
        let h = tree.latest_height().unwrap_or(0);
        (h, [0u8; 32])
    };

    tracing::info!(height = follow_height, "entering follow mode");

    #[cfg(feature = "nullifier")]
    let nf_scenario = pir_types::YpirScenario {
        num_items: NUM_BUCKETS as u64,
        item_size_bits: (BUCKET_BYTES * 8) as u64,
    };

    #[cfg(feature = "witness")]
    let wit_scenario = pir_types::YpirScenario {
        num_items: L0_DB_ROWS as u64,
        item_size_bits: (SUBSHARD_ROW_BYTES * 8) as u64,
    };

    #[cfg(feature = "decryption")]
    let dec_scenario = pir_types::YpirScenario {
        num_items: DECRYPT_DB_ROWS as u64,
        item_size_bits: (DECRYPT_ROW_BYTES * 8) as u64,
    };

    let mut client = LwdClient::connect(&config.lwd_urls).await?;
    let mut tracker =
        ChainTracker::with_tip(follow_height, follow_hash, CONFIRMATION_DEPTH as usize * 2);
    let mut current_height = follow_height;
    let mut blocks_since_snapshot: u64 = 0;
    #[cfg(feature = "nullifier")]
    let mut nf_prev_tree_size: Option<u32> = None;

    loop {
        let (tip_height, _) = client.get_latest_block().await?;

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

            #[cfg(feature = "nullifier")]
            let (nullifiers, nf_this_tree_size) =
                nf_ingest::extract_nullifiers_with_meta(block, nf_prev_tree_size);
            #[cfg(all(feature = "witness", not(feature = "decryption")))]
            let commitments = commitment_ingest::extract_commitments(block);
            #[cfg(feature = "decryption")]
            let (commitments, dec_leaves) =
                commitment_ingest::extract_commitments_and_decryption(block);

            match tracker.push_block(height, hash, prev_hash) {
                ChainAction::Extend => {
                    #[cfg(feature = "nullifier")]
                    {
                        if let Err(e) = hashtable.insert_block(height, hash, &nullifiers) {
                            tracing::warn!(height, error = %e, "nullifier insert failed");
                        }
                        hashtable.evict_to_target();
                        nf_prev_tree_size = nf_this_tree_size;
                    }

                    #[cfg(feature = "witness")]
                    {
                        tree.append_commitments(height, hash, &commitments);
                    }

                    #[cfg(feature = "decryption")]
                    {
                        dec_db.append_leaves(height, hash, &dec_leaves);
                    }

                    current_height = height;
                    blocks_since_snapshot += 1;

                    #[cfg(all(feature = "nullifier", feature = "witness"))]
                    tracing::info!(
                        height,
                        nfs = nullifiers.len(),
                        cmx = commitments.len(),
                        tree_size = tree.tree_size(),
                        "new block"
                    );
                    #[cfg(all(feature = "nullifier", not(feature = "witness")))]
                    tracing::info!(height, nfs = nullifiers.len(), "new block");
                    #[cfg(all(feature = "witness", not(feature = "nullifier")))]
                    tracing::info!(
                        height,
                        cmx = commitments.len(),
                        tree_size = tree.tree_size(),
                        "new block"
                    );
                }
                ChainAction::Reorg { rollback_to } => {
                    #[cfg(feature = "nullifier")]
                    {
                        while hashtable.latest_height().is_some_and(|h| h > rollback_to) {
                            if let Some(bh) = hashtable.latest_block_hash() {
                                if let Err(e) = hashtable.rollback_block(&bh) {
                                    tracing::warn!(error = %e, "nullifier rollback failed");
                                    break;
                                }
                            } else {
                                break;
                            }
                        }
                        if let Err(e) = hashtable.insert_block(height, hash, &nullifiers) {
                            tracing::warn!(height, error = %e, "nullifier insert after reorg failed");
                        }
                        nf_prev_tree_size = nf_this_tree_size;
                    }

                    #[cfg(feature = "witness")]
                    {
                        tree.rollback_to(rollback_to);
                        tree.append_commitments(height, hash, &commitments);
                    }

                    #[cfg(feature = "decryption")]
                    {
                        dec_db.rollback_to(rollback_to);
                        dec_db.append_leaves(height, hash, &dec_leaves);
                    }

                    current_height = height;
                    blocks_since_snapshot += 1;
                    #[cfg(feature = "witness")]
                    tracing::info!(
                        rollback_to,
                        new_height = height,
                        tree_size = tree.tree_size(),
                        "reorg handled"
                    );
                    #[cfg(not(feature = "witness"))]
                    tracing::info!(rollback_to, new_height = height, "reorg handled");
                }
            }
        }

        // Rebuild PIR databases
        #[cfg(feature = "nullifier")]
        {
            let nf_pir = spend_server::server::rebuild_pir(&*nf_engine, &hashtable, &nf_scenario)
                .map_err(ServerError::Nullifier)?;
            nf_state.live_pir.store(Arc::new(Some(nf_pir)));
        }

        #[cfg(feature = "witness")]
        {
            let anchor_height = tree.latest_height().unwrap_or(0);
            let wit_pir = witness_server::server::rebuild_pir(
                &*wit_engine,
                &mut tree,
                &wit_scenario,
                anchor_height,
            )
            .map_err(ServerError::Witness)?;
            wit_state.live_pir.store(Arc::new(Some(wit_pir)));
        }

        #[cfg(feature = "decryption")]
        {
            let anchor_height = dec_db.latest_height().unwrap_or(0);
            let dec_pir = decryption_server::server::rebuild_pir(
                &*dec_engine,
                &dec_db,
                &dec_scenario,
                anchor_height,
            )?;
            dec_state.live_pir.store(Arc::new(Some(dec_pir)));
        }

        // Periodic snapshots
        if blocks_since_snapshot >= config.snapshot_interval {
            #[cfg(feature = "nullifier")]
            {
                let nf_data_dir = config.data_dir.join("nullifier");
                spend_server::snapshot_io::save_snapshot(&hashtable, &nf_data_dir)
                    .await
                    .map_err(spend_server::server::ServerError::from)
                    .map_err(ServerError::Nullifier)?;
            }
            #[cfg(feature = "witness")]
            {
                let wit_data_dir = config.data_dir.join("witness");
                witness_server::snapshot_io::save_snapshot(&tree, &wit_data_dir)
                    .await
                    .map_err(witness_server::server::ServerError::from)
                    .map_err(ServerError::Witness)?;
            }
            #[cfg(feature = "decryption")]
            {
                let dec_data_dir = config.data_dir.join("decryption");
                decryption_server::snapshot_io::save_snapshot(&dec_db, &dec_data_dir)
                    .await
                    .map_err(decryption_server::server::ServerError::from)?;
            }
            blocks_since_snapshot = 0;
            tracing::info!("periodic snapshots saved");
        }

        sleep(FOLLOW_POLL_INTERVAL).await;
    }
}

#[cfg(feature = "nullifier")]
async fn catch_up_nullifier(
    lwd_urls: &[String],
    from: u64,
    to: u64,
    hashtable: &mut HashTableDb,
) -> Result<()> {
    let phase = arc_swap::ArcSwap::from_pointee(ServerPhase::Serving);
    spend_server::server::sync_range(lwd_urls, from, to, hashtable, None, &phase)
        .await
        .map_err(ServerError::Nullifier)?;
    hashtable.evict_to_target();
    Ok(())
}

#[cfg(feature = "witness")]
async fn catch_up_witness(
    lwd_urls: &[String],
    from: u64,
    to: u64,
    tree: &mut CommitmentTreeDb,
    initial_tree_size: Option<u32>,
) -> Result<()> {
    let phase = arc_swap::ArcSwap::from_pointee(ServerPhase::Serving);
    witness_server::server::sync_range(lwd_urls, from, to, tree, initial_tree_size, &phase)
        .await
        .map_err(ServerError::Witness)?;
    Ok(())
}

/// Determine the block height to start syncing decryption data from.
///
/// For a windowed DB (leaf_offset > 0), finds the completing block height via
/// GetSubtreeRoots — the same block the witness server starts from. For a
/// full-range DB, starts from NU5.
#[cfg(feature = "decryption")]
async fn decryption_sync_start(lwd_urls: &[String], leaf_offset: u64) -> Result<u64> {
    use witness_types::SHARD_LEAVES;

    if leaf_offset == 0 {
        return Ok(pir_types::NU5_MAINNET_ACTIVATION);
    }

    let mut client = LwdClient::connect(lwd_urls).await?;
    let subtree_roots = client.get_subtree_roots(1, 0, 65535).await?;
    let window_start_shard = (leaf_offset / SHARD_LEAVES as u64) as usize;

    if window_start_shard > 0 && window_start_shard <= subtree_roots.len() {
        Ok(subtree_roots[window_start_shard - 1].completing_block_height)
    } else {
        Ok(pir_types::NU5_MAINNET_ACTIVATION)
    }
}

/// Fetch blocks in batches and append decryption leaves to the DB.
///
/// Handles the windowed case: if the DB is empty and has a non-zero offset,
/// the first block's actions are split — only leaves past the offset are kept.
#[cfg(feature = "decryption")]
async fn catch_up_decryption(
    lwd_urls: &[String],
    from: u64,
    to: u64,
    dec_db: &mut DecryptionDb,
) -> Result<()> {
    const BATCH_SIZE: u64 = 10_000;
    let mut client = LwdClient::connect(lwd_urls).await?;
    let mut current = from;
    let leaf_offset = dec_db.leaf_offset();

    while current <= to {
        let batch_end = (current + BATCH_SIZE - 1).min(to);
        let blocks = client.get_block_range(current, batch_end).await?;

        for block in &blocks {
            let all_leaves = commitment_ingest::extract_decryption_leaves(block);
            let height = block.height;
            let hash = to_hash_array(&block.hash);

            if dec_db.tree_size() == 0 && leaf_offset > 0 {
                // First block in a windowed sync: only keep actions past
                // the offset. Requires chain_metadata to determine the split;
                // if metadata is absent we cannot safely determine which
                // actions belong in the window so we skip the block.
                let end_tree_size = block
                    .chain_metadata
                    .as_ref()
                    .map(|m| m.orchard_commitment_tree_size as u64);
                match end_tree_size {
                    Some(ets) if ets > leaf_offset => {
                        let spillover_count = (ets - leaf_offset) as usize;
                        let skip = all_leaves.len().saturating_sub(spillover_count);
                        dec_db.append_leaves(height, hash, &all_leaves[skip..]);
                    }
                    Some(_) => continue,
                    None => {
                        tracing::warn!(
                            height,
                            "block missing orchard_commitment_tree_size, skipping"
                        );
                        continue;
                    }
                }
            } else {
                dec_db.append_leaves(height, hash, &all_leaves);
            }
        }

        if current % 10_000 == from % 10_000 || batch_end >= to {
            tracing::info!(
                height = batch_end,
                tree_size = dec_db.tree_size(),
                "decryption sync progress"
            );
        }

        current = batch_end + 1;
    }

    Ok(())
}

fn to_hash_array(bytes: &[u8]) -> [u8; 32] {
    let mut arr = [0u8; 32];
    let len = bytes.len().min(32);
    arr[..len].copy_from_slice(&bytes[..len]);
    arr
}
