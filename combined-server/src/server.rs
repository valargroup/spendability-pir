use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use chain_ingest::{ChainAction, ChainTracker, LwdClient};
use commitment_tree_db::CommitmentTreeDb;
use hashtable_pir::HashTableDb;
use pir_types::{PirEngine, ServerPhase, CONFIRMATION_DEPTH};
use serde::Serialize;
use spend_types::{BUCKET_BYTES, NUM_BUCKETS};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use witness_types::{L0_DB_ROWS, SUBSHARD_ROW_BYTES};

const FOLLOW_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct CombinedConfig {
    pub target_size: usize,
    pub snapshot_interval: u64,
    pub data_dir: PathBuf,
    pub lwd_urls: Vec<String>,
    pub listen_addr: SocketAddr,
}

#[derive(thiserror::Error, Debug)]
pub enum ServerError {
    #[error("nullifier server error: {0}")]
    Nullifier(#[from] spend_server::server::ServerError),
    #[error("witness server error: {0}")]
    Witness(#[from] witness_server::server::ServerError),
    #[error("chain client error: {0}")]
    Client(#[from] chain_ingest::ClientError),
    #[error("hashtable error: {0}")]
    HashTable(#[from] hashtable_pir::HashTableError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ServerError>;

struct CombinedHealthState {
    nf_phase: Arc<arc_swap::ArcSwap<ServerPhase>>,
    wit_phase: Arc<arc_swap::ArcSwap<ServerPhase>>,
}

#[derive(Serialize)]
struct CombinedHealthResponse {
    nullifier: SubsystemHealth,
    witness: SubsystemHealth,
}

#[derive(Serialize)]
struct SubsystemHealth {
    phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_height: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_height: Option<u64>,
}

async fn combined_health(State(state): State<Arc<CombinedHealthState>>) -> impl IntoResponse {
    let nf_phase = state.nf_phase.load();
    let wit_phase = state.wit_phase.load();

    let nf_health = match nf_phase.as_ref() {
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
    };

    let wit_health = match wit_phase.as_ref() {
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
    };

    let both_serving = matches!(nf_phase.as_ref(), ServerPhase::Serving)
        && matches!(wit_phase.as_ref(), ServerPhase::Serving);

    let body = CombinedHealthResponse {
        nullifier: nf_health,
        witness: wit_health,
    };

    let status = if both_serving {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (status, axum::Json(body))
}

/// Main entry point for the combined PIR server.
pub async fn run<NfP, WitP>(
    config: CombinedConfig,
    nf_engine: Arc<NfP>,
    wit_engine: Arc<WitP>,
) -> Result<()>
where
    NfP: PirEngine + 'static,
    WitP: PirEngine + 'static,
{
    let nf_data_dir = config.data_dir.join("nullifier");
    let wit_data_dir = config.data_dir.join("witness");

    let nf_config = spend_server::state::ServerConfig {
        target_size: config.target_size,
        confirmation_depth: CONFIRMATION_DEPTH,
        snapshot_interval: config.snapshot_interval,
        data_dir: nf_data_dir.clone(),
        lwd_urls: config.lwd_urls.clone(),
        listen_addr: config.listen_addr,
    };

    let wit_config = witness_server::state::ServerConfig {
        snapshot_interval: config.snapshot_interval,
        data_dir: wit_data_dir.clone(),
        lwd_urls: config.lwd_urls.clone(),
        listen_addr: config.listen_addr,
    };

    // Run both sync phases concurrently. Each creates its own LwdClient
    // and uses its subsystem-specific sync strategy.
    tracing::info!("starting concurrent sync for nullifier and witness subsystems");

    let nf_sync = spend_server::server::run_sync_only(nf_config.clone(), nf_engine.clone());
    let wit_sync = witness_server::server::run_sync_only(wit_config.clone(), wit_engine.clone());

    let (nf_result, wit_result) = tokio::join!(nf_sync, wit_sync);

    let (nf_state, mut hashtable) = nf_result.map_err(ServerError::Nullifier)?;
    let (wit_state, mut tree) = wit_result.map_err(ServerError::Witness)?;

    tracing::info!(
        nf_height = hashtable.latest_height(),
        nf_nullifiers = hashtable.len(),
        wit_height = tree.latest_height(),
        wit_tree_size = tree.tree_size(),
        "both subsystems synced, building combined router",
    );

    // Build combined router: subsystem routes under prefixes, shared health at root
    let health_state = Arc::new(CombinedHealthState {
        nf_phase: Arc::new(arc_swap::ArcSwap::from_pointee(ServerPhase::Serving)),
        wit_phase: Arc::new(arc_swap::ArcSwap::from_pointee(ServerPhase::Serving)),
    });

    let router = Router::new()
        .route("/health", get(combined_health))
        .with_state(health_state)
        .nest(
            "/nullifier",
            spend_server::server::build_router(nf_state.clone()),
        )
        .nest(
            "/witness",
            witness_server::server::build_router(wit_state.clone()),
        );

    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!(listen = %config.listen_addr, "http server started");
    let _http_handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    // Unified follow loop: single lightwalletd connection, dual dispatch
    let nf_latest = hashtable.latest_height().unwrap_or(0);
    let wit_latest = tree.latest_height().unwrap_or(0);

    // If the two subsystems ended at different heights, catch up the one
    // that's behind. This uses their respective sync functions.
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
            tracing::info!(
                from = wit_latest + 1,
                to = nf_latest,
                "catching up witness subsystem"
            );
            let ts = if tree.tree_size() > 0 {
                Some(tree.tree_size() as u32)
            } else {
                None
            };
            catch_up_witness(&config.lwd_urls, wit_latest + 1, nf_latest, &mut tree, ts).await?;
        }
        std::cmp::Ordering::Equal => {}
    }

    let follow_height = hashtable.latest_height().unwrap_or(0);
    let follow_hash = hashtable.latest_block_hash().unwrap_or([0u8; 32]);

    tracing::info!(height = follow_height, "entering unified follow mode");

    let nf_scenario = pir_types::YpirScenario {
        num_items: NUM_BUCKETS as u64,
        item_size_bits: (BUCKET_BYTES * 8) as u64,
    };
    let wit_scenario = pir_types::YpirScenario {
        num_items: L0_DB_ROWS as u64,
        item_size_bits: (SUBSHARD_ROW_BYTES * 8) as u64,
    };

    let mut client = LwdClient::connect(&config.lwd_urls).await?;
    let mut tracker =
        ChainTracker::with_tip(follow_height, follow_hash, CONFIRMATION_DEPTH as usize * 2);
    let mut current_height = follow_height;
    let mut blocks_since_snapshot: u64 = 0;

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
            let nullifiers = nf_ingest::extract_nullifiers(block);
            let commitments = commitment_ingest::extract_commitments(block);
            match tracker.push_block(height, hash, prev_hash) {
                ChainAction::Extend => {
                    // Nullifier subsystem
                    if let Err(e) = hashtable.insert_block(height, hash, &nullifiers) {
                        tracing::warn!(height, error = %e, "nullifier insert failed");
                    }
                    hashtable.evict_to_target();

                    // Witness subsystem
                    tree.append_commitments(height, hash, &commitments);

                    current_height = height;
                    blocks_since_snapshot += 1;

                    tracing::info!(
                        height,
                        nfs = nullifiers.len(),
                        cmx = commitments.len(),
                        tree_size = tree.tree_size(),
                        "new block",
                    );
                }
                ChainAction::Reorg { rollback_to } => {
                    // Roll back nullifier: remove blocks above rollback_to by hash
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

                    // Roll back witness
                    tree.rollback_to(rollback_to);

                    // Insert the new block into both
                    if let Err(e) = hashtable.insert_block(height, hash, &nullifiers) {
                        tracing::warn!(height, error = %e, "nullifier insert after reorg failed");
                    }
                    tree.append_commitments(height, hash, &commitments);

                    current_height = height;
                    blocks_since_snapshot += 1;
                    tracing::info!(
                        rollback_to,
                        new_height = height,
                        tree_size = tree.tree_size(),
                        "reorg handled",
                    );
                }
            }
        }

        // Rebuild both PIR databases
        let nf_pir = spend_server::server::rebuild_pir(&*nf_engine, &hashtable, &nf_scenario)
            .map_err(ServerError::Nullifier)?;
        nf_state.live_pir.store(Arc::new(Some(nf_pir)));

        let anchor_height = tree.latest_height().unwrap_or(0);
        let wit_pir = witness_server::server::rebuild_pir(
            &*wit_engine,
            &mut tree,
            &wit_scenario,
            anchor_height,
        )
        .map_err(ServerError::Witness)?;
        wit_state.live_pir.store(Arc::new(Some(wit_pir)));

        // Periodic snapshots for both
        if blocks_since_snapshot >= config.snapshot_interval {
            spend_server::snapshot_io::save_snapshot(&hashtable, &nf_data_dir)
                .await
                .map_err(spend_server::server::ServerError::from)
                .map_err(ServerError::Nullifier)?;
            witness_server::snapshot_io::save_snapshot(&tree, &wit_data_dir)
                .await
                .map_err(witness_server::server::ServerError::from)
                .map_err(ServerError::Witness)?;
            blocks_since_snapshot = 0;
            tracing::info!("periodic snapshots saved");
        }

        sleep(FOLLOW_POLL_INTERVAL).await;
    }
}

/// Catch up the nullifier subsystem to a target height.
async fn catch_up_nullifier(
    lwd_urls: &[String],
    from: u64,
    to: u64,
    hashtable: &mut HashTableDb,
) -> Result<()> {
    let phase = arc_swap::ArcSwap::from_pointee(ServerPhase::Serving);
    spend_server::server::sync_range(lwd_urls, from, to, hashtable, &phase)
        .await
        .map_err(ServerError::Nullifier)?;
    hashtable.evict_to_target();
    Ok(())
}

/// Catch up the witness subsystem to a target height.
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

fn to_hash_array(bytes: &[u8]) -> [u8; 32] {
    let mut arr = [0u8; 32];
    let len = bytes.len().min(32);
    arr[..len].copy_from_slice(&bytes[..len]);
    arr
}
