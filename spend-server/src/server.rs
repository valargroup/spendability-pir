use crate::routes;
use crate::snapshot_io;
use crate::state::{AppState, PirState, ServerConfig};
use axum::routing::{get, post};
use axum::Router;
use hashtable_pir::HashTableDb;
use spend_types::{ChainEvent, PirEngine, ServerPhase, SpendabilityMetadata, NUM_BUCKETS};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(thiserror::Error, Debug)]
pub enum ServerError {
    #[error("ingest error: {0}")]
    Ingest(#[from] nf_ingest::ingest::IngestError),
    #[error("hashtable error: {0}")]
    HashTable(#[from] hashtable_pir::HashTableError),
    #[error("snapshot io error: {0}")]
    SnapshotIo(#[from] snapshot_io::SnapshotIoError),
    #[error("pir setup failed: {0}")]
    PirSetup(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ServerError>;

/// Build the Axum router for the given AppState.
pub fn build_router<P: PirEngine + 'static>(state: Arc<AppState<P>>) -> Router {
    Router::new()
        .route("/health", get(routes::health::<P>))
        .route("/metadata", get(routes::metadata::<P>))
        .route("/params", get(routes::params::<P>))
        .route("/query", post(routes::query::<P>))
        .with_state(state)
}

/// Build PIR server state from the current hash table and store it in the ArcSwap.
fn rebuild_pir<P: PirEngine>(
    engine: &P,
    hashtable: &HashTableDb,
    scenario: &spend_types::YpirScenario,
) -> std::result::Result<PirState<P>, ServerError> {
    let total_start = std::time::Instant::now();

    let serialize_start = std::time::Instant::now();
    let db_bytes = hashtable.to_pir_bytes();
    let serialize_ms = serialize_start.elapsed().as_millis();

    let setup_start = std::time::Instant::now();
    let engine_state = engine
        .setup(&db_bytes, scenario)
        .map_err(|e| ServerError::PirSetup(e.to_string()))?;
    let setup_ms = setup_start.elapsed().as_millis();

    let metadata = SpendabilityMetadata {
        earliest_height: hashtable.earliest_height().unwrap_or(0),
        latest_height: hashtable.latest_height().unwrap_or(0),
        num_nullifiers: hashtable.len() as u64,
        num_buckets: NUM_BUCKETS as u64,
        phase: ServerPhase::Serving,
    };

    tracing::info!(
        total_ms = total_start.elapsed().as_millis() as u64,
        serialize_ms = serialize_ms as u64,
        setup_ms = setup_ms as u64,
        db_bytes = db_bytes.len(),
        nullifiers = metadata.num_nullifiers,
        height_range = format_args!("{}..{}", metadata.earliest_height, metadata.latest_height),
        "pir rebuild complete",
    );

    Ok(PirState {
        engine_state,
        metadata,
    })
}

/// Main server entry point. Runs sync mode, transitions to follow mode, serves HTTP.
pub async fn run<P: PirEngine + 'static>(config: ServerConfig, engine: Arc<P>) -> Result<()> {
    let app_state = Arc::new(AppState::new(config.clone(), engine.clone()));

    // Start HTTP server immediately (returns 503 during sync)
    let router = build_router(app_state.clone());
    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    let http_handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    // Connect to lightwalletd
    let mut client = nf_ingest::LwdClient::connect(&config.lwd_urls)
        .await
        .map_err(nf_ingest::ingest::IngestError::from)?;

    let (tip_height, _) = client
        .get_latest_block()
        .await
        .map_err(nf_ingest::ingest::IngestError::from)?;

    // Load snapshot or fresh start
    let (mut hashtable, sync_from) = match snapshot_io::load_snapshot(&config.data_dir).await {
        Ok(ht) => {
            let resume = ht.latest_height().map(|h| h + 1).unwrap_or(0);
            tracing::info!(resume_height = resume, "loaded snapshot");
            (ht, resume)
        }
        Err(_) => {
            let blocks_needed = config.target_size as u64 / 20;
            let start = tip_height.saturating_sub(blocks_needed);
            tracing::info!(start_height = start, "fresh start");
            (HashTableDb::new(), start)
        }
    };

    // Sync mode
    if sync_from <= tip_height {
        app_state.phase.store(Arc::new(ServerPhase::Syncing {
            current_height: sync_from,
            target_height: tip_height,
        }));
        tracing::info!(from = sync_from, to = tip_height, "entering sync mode");

        let (tx, mut rx) = mpsc::channel::<ChainEvent>(1000);
        let sync_handle = {
            let mut sync_client = nf_ingest::LwdClient::connect(&config.lwd_urls)
                .await
                .map_err(nf_ingest::ingest::IngestError::from)?;
            tokio::spawn(async move {
                nf_ingest::ingest::sync(&mut sync_client, sync_from, tip_height, &tx).await
            })
        };

        while let Some(event) = rx.recv().await {
            if let ChainEvent::NewBlock {
                height,
                hash,
                nullifiers,
                ..
            } = event
            {
                hashtable.insert_block(height, hash, &nullifiers)?;
                hashtable.evict_to_target();

                if height % 1000 == 0 {
                    app_state.phase.store(Arc::new(ServerPhase::Syncing {
                        current_height: height,
                        target_height: tip_height,
                    }));
                    tracing::info!(height, "sync progress");
                }
            }
        }

        sync_handle.await.ok();

        // Save snapshot after sync
        snapshot_io::save_snapshot(&hashtable, &config.data_dir).await?;
        tracing::info!("snapshot saved after sync");
    }

    // Build PIR once and start serving
    let pir_state = rebuild_pir(&*engine, &hashtable, &app_state.scenario)?;
    app_state.live_pir.store(Arc::new(Some(pir_state)));
    app_state.phase.store(Arc::new(ServerPhase::Serving));
    tracing::info!(
        height = hashtable.latest_height(),
        nullifiers = hashtable.len(),
        "serving"
    );

    // Follow mode
    let latest_height = hashtable.latest_height().unwrap_or(0);
    let latest_hash = hashtable.latest_block_hash().unwrap_or([0u8; 32]);

    let (tx, mut rx) = mpsc::channel::<ChainEvent>(100);
    let follow_handle = {
        let mut follow_client = nf_ingest::LwdClient::connect(&config.lwd_urls)
            .await
            .map_err(nf_ingest::ingest::IngestError::from)?;
        tokio::spawn(async move {
            nf_ingest::ingest::follow(&mut follow_client, latest_height, latest_hash, &tx).await
        })
    };

    let mut blocks_since_snapshot: u64 = 0;

    while let Some(event) = rx.recv().await {
        match event {
            ChainEvent::NewBlock {
                height,
                hash,
                nullifiers,
                ..
            } => {
                hashtable.insert_block(height, hash, &nullifiers)?;
                hashtable.evict_to_target();
                blocks_since_snapshot += 1;
                tracing::info!(height, nfs = nullifiers.len(), "new block");
            }
            ChainEvent::Reorg {
                orphaned,
                new_blocks,
            } => {
                for block in orphaned.iter().rev() {
                    if let Err(e) = hashtable.rollback_block(&block.hash) {
                        tracing::warn!(height = block.height, error = %e, "rollback failed, skipping");
                    }
                }
                for block in &new_blocks {
                    hashtable.insert_block(block.height, block.hash, &block.nullifiers)?;
                }
                hashtable.evict_to_target();
                blocks_since_snapshot += 1;
                tracing::info!(
                    orphaned = orphaned.len(),
                    new = new_blocks.len(),
                    "reorg handled"
                );
            }
        }

        // Rebuild PIR and atomic swap
        let pir_state = rebuild_pir(&*engine, &hashtable, &app_state.scenario)?;
        app_state.live_pir.store(Arc::new(Some(pir_state)));

        // Periodic snapshot
        if blocks_since_snapshot >= config.snapshot_interval {
            snapshot_io::save_snapshot(&hashtable, &config.data_dir).await?;
            blocks_since_snapshot = 0;
            tracing::info!("periodic snapshot saved");
        }
    }

    follow_handle.abort();
    http_handle.abort();
    Ok(())
}

/// Simplified runner for testing: runs sync, builds PIR, returns the app state
/// without entering the follow loop. Caller can then hit HTTP routes directly.
pub async fn run_sync_only<P: PirEngine + 'static>(
    config: ServerConfig,
    engine: Arc<P>,
) -> Result<(Arc<AppState<P>>, HashTableDb)> {
    let app_state = Arc::new(AppState::new(config.clone(), engine.clone()));

    let mut client = nf_ingest::LwdClient::connect(&config.lwd_urls)
        .await
        .map_err(nf_ingest::ingest::IngestError::from)?;

    let (tip_height, _) = client
        .get_latest_block()
        .await
        .map_err(nf_ingest::ingest::IngestError::from)?;

    let (mut hashtable, sync_from) = match snapshot_io::load_snapshot(&config.data_dir).await {
        Ok(ht) => {
            let resume = ht.latest_height().map(|h| h + 1).unwrap_or(0);
            (ht, resume)
        }
        Err(_) => {
            let blocks_needed = config.target_size as u64 / 20;
            let start = tip_height.saturating_sub(blocks_needed);
            (HashTableDb::new(), start)
        }
    };

    if sync_from <= tip_height {
        app_state.phase.store(Arc::new(ServerPhase::Syncing {
            current_height: sync_from,
            target_height: tip_height,
        }));

        let (tx, mut rx) = mpsc::channel::<ChainEvent>(1000);
        let sync_handle = {
            let mut sync_client = nf_ingest::LwdClient::connect(&config.lwd_urls)
                .await
                .map_err(nf_ingest::ingest::IngestError::from)?;
            tokio::spawn(async move {
                nf_ingest::ingest::sync(&mut sync_client, sync_from, tip_height, &tx).await
            })
        };

        while let Some(event) = rx.recv().await {
            if let ChainEvent::NewBlock {
                height,
                hash,
                nullifiers,
                ..
            } = event
            {
                hashtable.insert_block(height, hash, &nullifiers)?;
                hashtable.evict_to_target();
            }
        }
        sync_handle.await.ok();

        snapshot_io::save_snapshot(&hashtable, &config.data_dir).await?;
    }

    let pir_state = rebuild_pir(&*engine, &hashtable, &app_state.scenario)?;
    app_state.live_pir.store(Arc::new(Some(pir_state)));
    app_state.phase.store(Arc::new(ServerPhase::Serving));

    Ok((app_state, hashtable))
}
