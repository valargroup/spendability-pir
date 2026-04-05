use crate::routes;
use crate::snapshot_io;
use crate::state::{AppState, PirState, ServerConfig, WitnessMetadata};
use axum::routing::{get, post};
use axum::Router;
use commitment_tree_db::CommitmentTreeDb;
use pir_types::{PirEngine, ServerPhase, NU5_MAINNET_ACTIVATION};
use std::sync::Arc;
use tokio::sync::mpsc;
use witness_types::{WitnessChainEvent, L0_MAX_SHARDS, SHARD_LEAVES};

const ORCHARD_PROTOCOL: i32 = 1;

#[derive(thiserror::Error, Debug)]
pub enum ServerError {
    #[error("ingest error: {0}")]
    Ingest(Box<commitment_ingest::ingest::IngestError>),
    #[error("snapshot io error: {0}")]
    SnapshotIo(#[from] snapshot_io::SnapshotIoError),
    #[error("pir setup failed: {0}")]
    PirSetup(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("chain client error: {0}")]
    Client(Box<chain_ingest::ClientError>),
}

impl From<chain_ingest::ClientError> for ServerError {
    fn from(e: chain_ingest::ClientError) -> Self {
        ServerError::Client(Box::new(e))
    }
}

impl From<commitment_ingest::ingest::IngestError> for ServerError {
    fn from(e: commitment_ingest::ingest::IngestError) -> Self {
        ServerError::Ingest(Box::new(e))
    }
}

pub type Result<T> = std::result::Result<T, ServerError>;

/// Build the Axum router for the given AppState.
pub fn build_router<P: PirEngine + 'static>(state: Arc<AppState<P>>) -> Router {
    Router::new()
        .route("/health", get(routes::health::<P>))
        .route("/metadata", get(routes::metadata::<P>))
        .route("/broadcast", get(routes::broadcast::<P>))
        .route("/params", get(routes::params::<P>))
        .route("/query", post(routes::query::<P>))
        .with_state(state)
}

/// Build PIR server state from the current commitment tree and store it.
pub fn rebuild_pir<P: PirEngine>(
    engine: &P,
    tree: &mut CommitmentTreeDb,
    scenario: &pir_types::YpirScenario,
    anchor_height: u64,
) -> std::result::Result<PirState<P>, ServerError> {
    let total_start = std::time::Instant::now();

    let build_start = std::time::Instant::now();
    let (db_bytes, broadcast) = tree.build_pir_db_and_broadcast(anchor_height);
    let build_ms = build_start.elapsed().as_millis();

    let setup_start = std::time::Instant::now();
    let engine_state = engine
        .setup(&db_bytes, scenario)
        .map_err(|e| ServerError::PirSetup(e.to_string()))?;
    let setup_ms = setup_start.elapsed().as_millis();

    let metadata = WitnessMetadata {
        anchor_height,
        tree_size: tree.tree_size(),
        window_start_shard: tree.window_start_shard(),
        window_shard_count: tree.window_shard_count(),
        populated_shards: tree.populated_shards(),
        phase: ServerPhase::Serving,
    };

    tracing::info!(
        total_ms = total_start.elapsed().as_millis() as u64,
        build_ms = build_ms as u64,
        setup_ms = setup_ms as u64,
        db_bytes = db_bytes.len(),
        tree_size = metadata.tree_size,
        shards = metadata.populated_shards,
        window = format_args!(
            "{}..+{}",
            metadata.window_start_shard, metadata.window_shard_count
        ),
        anchor_height,
        "pir rebuild complete",
    );

    Ok(PirState {
        engine_state,
        broadcast,
        metadata,
    })
}

/// Determine the sync start point using GetSubtreeRoots.
///
/// Returns `(CommitmentTreeDb, sync_from_height, initial_tree_size)`.
/// If there are enough completed shards, creates a windowed tree with
/// prefetched roots and syncs only the window. Otherwise syncs from NU5.
async fn prepare_tree(
    client: &mut chain_ingest::LwdClient,
    tip_height: u64,
) -> Result<(CommitmentTreeDb, u64, Option<u32>)> {
    let subtree_roots = client.get_subtree_roots(ORCHARD_PROTOCOL, 0, 65535).await?;
    let num_completed = subtree_roots.len();

    tracing::info!(
        completed_shards = num_completed,
        "fetched subtree roots from lightwalletd"
    );

    if num_completed >= L0_MAX_SHARDS {
        // Window: keep the last (L0_MAX_SHARDS - 1) completed shards + frontier
        let window_start = num_completed - (L0_MAX_SHARDS - 1);
        let leaf_offset = (window_start as u64) * (SHARD_LEAVES as u64);

        let prefetched: Vec<[u8; 32]> = subtree_roots[..window_start]
            .iter()
            .map(|sr| {
                let mut root = [0u8; 32];
                let len = sr.root_hash.len().min(32);
                root[..len].copy_from_slice(&sr.root_hash[..len]);
                root
            })
            .collect();

        let sync_from = subtree_roots[window_start - 1].completing_block_height + 1;
        let initial_tree_size = Some(leaf_offset as u32);

        tracing::info!(
            window_start_shard = window_start,
            prefetched_roots = prefetched.len(),
            sync_from,
            leaf_offset,
            "using windowed sync (skipping {} shards)",
            window_start,
        );

        let tree = CommitmentTreeDb::with_offset(leaf_offset, prefetched);
        Ok((tree, sync_from, initial_tree_size))
    } else {
        let floor = min_sync_height(tip_height);
        tracing::info!(
            completed_shards = num_completed,
            sync_from = floor,
            "full sync from NU5 (fewer than {} completed shards)",
            L0_MAX_SHARDS,
        );
        Ok((CommitmentTreeDb::new(), floor, None))
    }
}

/// Lowest block height we'll ever sync.
fn min_sync_height(tip_height: u64) -> u64 {
    if tip_height >= NU5_MAINNET_ACTIVATION {
        NU5_MAINNET_ACTIVATION
    } else {
        1
    }
}

/// Sync a block range into the tree, reporting progress via `phase`.
pub async fn sync_range(
    lwd_urls: &[String],
    from: u64,
    to: u64,
    tree: &mut CommitmentTreeDb,
    initial_tree_size: Option<u32>,
    phase: &arc_swap::ArcSwap<ServerPhase>,
) -> Result<()> {
    if from > to {
        return Ok(());
    }

    let (tx, mut rx) = mpsc::channel::<WitnessChainEvent>(1000);
    let sync_handle = {
        let mut client = chain_ingest::LwdClient::connect(lwd_urls)
            .await
            .map_err(commitment_ingest::ingest::IngestError::from)?;
        let ts = initial_tree_size;
        tokio::spawn(async move {
            commitment_ingest::ingest::sync(&mut client, from, to, ts, &tx).await
        })
    };

    while let Some(event) = rx.recv().await {
        if let WitnessChainEvent::NewBlock {
            height,
            hash,
            commitments,
            ..
        } = event
        {
            tree.append_commitments(height, hash, &commitments);

            if height % 1000 == 0 {
                phase.store(Arc::new(ServerPhase::Syncing {
                    current_height: height,
                    target_height: to,
                }));
                tracing::info!(height, tree_size = tree.tree_size(), "sync progress");
            }
        }
    }

    sync_handle.await.ok();
    Ok(())
}

/// Main server entry point. Runs sync mode, transitions to follow mode, serves HTTP.
pub async fn run<P: PirEngine + 'static>(config: ServerConfig, engine: Arc<P>) -> Result<()> {
    let app_state = Arc::new(AppState::new(config.clone(), engine.clone()));

    // Try to bind early so health checks are available during sync.
    // Non-fatal: if the port is busy we'll sync + save snapshot and retry.
    let early_http = match tokio::net::TcpListener::bind(&config.listen_addr).await {
        Ok(listener) => {
            tracing::info!(listen = %config.listen_addr, "http server started (sync in progress)");
            let router = build_router(app_state.clone());
            Some(tokio::spawn(async move {
                axum::serve(listener, router).await.ok();
            }))
        }
        Err(e) => {
            tracing::warn!(addr = %config.listen_addr, error = %e, "port busy, will retry after sync");
            None
        }
    };

    let mut client = chain_ingest::LwdClient::connect(&config.lwd_urls)
        .await
        .map_err(commitment_ingest::ingest::IngestError::from)?;

    let (tip_height, _) = client
        .get_latest_block()
        .await
        .map_err(commitment_ingest::ingest::IngestError::from)?;

    // Try loading from snapshot first
    let (mut tree, forward_start, initial_tree_size) =
        match snapshot_io::load_snapshot(&config.data_dir).await {
            Ok(t) => {
                let resume = t.latest_height().map(|h| h + 1).unwrap_or(0);
                let ts = if t.tree_size() > 0 {
                    Some(t.tree_size() as u32)
                } else {
                    None
                };
                tracing::info!(
                    resume_height = resume,
                    tree_size = t.tree_size(),
                    leaf_offset = t.leaf_offset(),
                    "loaded snapshot"
                );
                (t, resume, ts)
            }
            Err(_) => {
                // No snapshot — use GetSubtreeRoots for smart sync
                prepare_tree(&mut client, tip_height).await?
            }
        };

    if forward_start <= tip_height {
        app_state.phase.store(Arc::new(ServerPhase::Syncing {
            current_height: forward_start,
            target_height: tip_height,
        }));
        tracing::info!(from = forward_start, to = tip_height, "entering sync mode");
        sync_range(
            &config.lwd_urls,
            forward_start,
            tip_height,
            &mut tree,
            initial_tree_size,
            &app_state.phase,
        )
        .await?;
    }

    tracing::info!(
        tree_size = tree.tree_size(),
        shards = tree.populated_shards(),
        window_start = tree.window_start_shard(),
        latest_height = tree.latest_height(),
        "sync complete",
    );

    snapshot_io::save_snapshot(&tree, &config.data_dir).await?;
    tracing::info!("snapshot saved after sync");

    let anchor_height = tree.latest_height().unwrap_or(0);
    let pir_state = rebuild_pir(&*engine, &mut tree, &app_state.scenario, anchor_height)?;
    app_state.live_pir.store(Arc::new(Some(pir_state)));
    app_state.phase.store(Arc::new(ServerPhase::Serving));
    tracing::info!(anchor_height, tree_size = tree.tree_size(), "serving");

    // Save snapshot again after rebuild so warm cache is persisted
    snapshot_io::save_snapshot(&tree, &config.data_dir).await?;
    tracing::info!("snapshot saved with warm cache");

    let http_handle = match early_http {
        Some(h) => h,
        None => {
            let router = build_router(app_state.clone());
            let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
            tracing::info!(listen = %config.listen_addr, "http server started");
            tokio::spawn(async move {
                axum::serve(listener, router).await.ok();
            })
        }
    };

    // Follow mode
    let latest_height = tree.latest_height().unwrap_or(0);
    let latest_hash = tree.latest_block_hash().unwrap_or([0u8; 32]);
    let follow_tree_size = if tree.tree_size() > 0 {
        Some(tree.tree_size() as u32)
    } else {
        None
    };

    let (tx, mut rx) = mpsc::channel::<WitnessChainEvent>(100);
    let follow_handle = {
        let mut follow_client = chain_ingest::LwdClient::connect(&config.lwd_urls)
            .await
            .map_err(commitment_ingest::ingest::IngestError::from)?;
        let ts = follow_tree_size;
        tokio::spawn(async move {
            commitment_ingest::ingest::follow(
                &mut follow_client,
                latest_height,
                latest_hash,
                ts,
                &tx,
            )
            .await
        })
    };

    let mut blocks_since_snapshot: u64 = 0;

    while let Some(event) = rx.recv().await {
        match event {
            WitnessChainEvent::NewBlock {
                height,
                hash,
                commitments,
                ..
            } => {
                tree.append_commitments(height, hash, &commitments);
                blocks_since_snapshot += 1;
                tracing::info!(
                    height,
                    cmx = commitments.len(),
                    tree_size = tree.tree_size(),
                    "new block"
                );
            }
            WitnessChainEvent::Reorg { rollback_to } => {
                tree.rollback_to(rollback_to);
                tracing::info!(rollback_to, tree_size = tree.tree_size(), "reorg handled");
            }
        }

        let anchor_height = tree.latest_height().unwrap_or(0);
        let pir_state = rebuild_pir(&*engine, &mut tree, &app_state.scenario, anchor_height)?;
        app_state.live_pir.store(Arc::new(Some(pir_state)));

        if blocks_since_snapshot >= config.snapshot_interval {
            snapshot_io::save_snapshot(&tree, &config.data_dir).await?;
            blocks_since_snapshot = 0;
            tracing::info!("periodic snapshot saved");
        }
    }

    follow_handle.abort();
    http_handle.abort();
    Ok(())
}

/// Simplified runner for testing: runs sync, builds PIR, returns the app state.
pub async fn run_sync_only<P: PirEngine + 'static>(
    config: ServerConfig,
    engine: Arc<P>,
) -> Result<(Arc<AppState<P>>, CommitmentTreeDb)> {
    let app_state = Arc::new(AppState::new(config.clone(), engine.clone()));

    let mut client = chain_ingest::LwdClient::connect(&config.lwd_urls)
        .await
        .map_err(commitment_ingest::ingest::IngestError::from)?;

    let (tip_height, _) = client
        .get_latest_block()
        .await
        .map_err(commitment_ingest::ingest::IngestError::from)?;

    let (mut tree, forward_start, initial_tree_size) =
        match snapshot_io::load_snapshot(&config.data_dir).await {
            Ok(t) => {
                let resume = t.latest_height().map(|h| h + 1).unwrap_or(0);
                let ts = if t.tree_size() > 0 {
                    Some(t.tree_size() as u32)
                } else {
                    None
                };
                (t, resume, ts)
            }
            Err(_) => prepare_tree(&mut client, tip_height).await?,
        };

    if forward_start <= tip_height {
        app_state.phase.store(Arc::new(ServerPhase::Syncing {
            current_height: forward_start,
            target_height: tip_height,
        }));
        sync_range(
            &config.lwd_urls,
            forward_start,
            tip_height,
            &mut tree,
            initial_tree_size,
            &app_state.phase,
        )
        .await?;
    }

    snapshot_io::save_snapshot(&tree, &config.data_dir).await?;

    let anchor_height = tree.latest_height().unwrap_or(0);
    let pir_state = rebuild_pir(&*engine, &mut tree, &app_state.scenario, anchor_height)?;
    app_state.live_pir.store(Arc::new(Some(pir_state)));
    app_state.phase.store(Arc::new(ServerPhase::Serving));

    Ok((app_state, tree))
}
