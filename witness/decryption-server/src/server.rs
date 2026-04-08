use crate::routes;
use crate::state::{AppState, DecryptionMetadata, PirState};
use axum::routing::{get, post};
use axum::Router;
use decryption_db::DecryptionDb;
use pir_types::{PirEngine, ServerPhase};
use std::sync::Arc;

#[derive(thiserror::Error, Debug)]
pub enum ServerError {
    #[error("pir setup failed: {0}")]
    PirSetup(String),
    #[error("snapshot io error: {0}")]
    SnapshotIo(#[from] crate::snapshot_io::SnapshotIoError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ServerError>;

pub fn build_router<P: PirEngine + 'static>(state: Arc<AppState<P>>) -> Router {
    Router::new()
        .route("/health", get(routes::health::<P>))
        .route("/metadata", get(routes::metadata::<P>))
        .route("/params", get(routes::params::<P>))
        .route("/query", post(routes::query::<P>))
        .with_state(state)
}

pub fn rebuild_pir<P: PirEngine>(
    engine: &P,
    db: &DecryptionDb,
    scenario: &pir_types::YpirScenario,
    anchor_height: u64,
) -> std::result::Result<PirState<P>, ServerError> {
    let total_start = std::time::Instant::now();

    let build_start = std::time::Instant::now();
    let db_bytes = db.build_pir_db();
    let build_ms = build_start.elapsed().as_millis();

    let setup_start = std::time::Instant::now();
    let engine_state = engine
        .setup(&db_bytes, scenario)
        .map_err(|e| ServerError::PirSetup(e.to_string()))?;
    let setup_ms = setup_start.elapsed().as_millis();

    let metadata = DecryptionMetadata {
        anchor_height,
        tree_size: db.tree_size(),
        window_start_shard: db.window_start_shard(),
        window_shard_count: db.window_shard_count(),
        populated_shards: db.populated_shards(),
        phase: ServerPhase::Serving,
    };

    tracing::info!(
        total_ms = total_start.elapsed().as_millis() as u64,
        build_ms = build_ms as u64,
        setup_ms = setup_ms as u64,
        db_bytes = db_bytes.len(),
        tree_size = metadata.tree_size,
        window = format_args!(
            "{}..+{}",
            metadata.window_start_shard, metadata.window_shard_count
        ),
        anchor_height,
        "decryption pir rebuild complete",
    );

    Ok(PirState {
        engine_state,
        metadata,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pir_stub::StubPirEngine;
    use decryption_types::DecryptionLeaf;

    fn make_leaf(byte: u8) -> DecryptionLeaf {
        DecryptionLeaf {
            nf: [byte; 32],
            ephemeral_key: [byte.wrapping_add(1); 32],
            ciphertext: [byte.wrapping_add(2); 52],
        }
    }

    #[test]
    fn rebuild_with_stub_engine() {
        let engine = StubPirEngine;
        let scenario = pir_types::YpirScenario {
            num_items: decryption_types::DECRYPT_DB_ROWS as u64,
            item_size_bits: (decryption_types::DECRYPT_ROW_BYTES * 8) as u64,
        };

        let mut db = DecryptionDb::new();
        db.append_leaves(100, [1u8; 32], &[make_leaf(0xAA)]);

        let pir_state = rebuild_pir(&engine, &db, &scenario, 100).unwrap();
        assert_eq!(pir_state.metadata.anchor_height, 100);
        assert_eq!(pir_state.metadata.tree_size, 1);
        assert_eq!(pir_state.metadata.populated_shards, 1);
        assert_eq!(pir_state.metadata.window_shard_count, 1);
        assert_eq!(pir_state.metadata.window_start_shard, 0);
    }

    #[tokio::test]
    async fn stub_server_round_trip() {
        let engine = Arc::new(StubPirEngine);
        let state = Arc::new(AppState::new(engine.clone()));

        let mut db = DecryptionDb::new();
        let leaf = make_leaf(0xBB);
        db.append_leaves(100, [1u8; 32], &[leaf]);

        let pir_state = rebuild_pir(&*engine, &db, &state.scenario, 100).unwrap();
        state.live_pir.store(Arc::new(Some(pir_state)));
        state.phase.store(Arc::new(ServerPhase::Serving));

        let router = build_router(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });

        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        // /health — verify all metadata fields
        let health: serde_json::Value = client
            .get(format!("{base}/health"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["phase"], "Serving");
        assert_eq!(health["anchor_height"], 100);
        assert_eq!(health["tree_size"], 1);
        assert_eq!(health["populated_shards"], 1);

        // /metadata — verify window geometry
        let meta: crate::state::DecryptionMetadata = client
            .get(format!("{base}/metadata"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(meta.anchor_height, 100);
        assert_eq!(meta.tree_size, 1);
        assert_eq!(meta.populated_shards, 1);
        assert_eq!(meta.window_start_shard, 0);
        assert_eq!(meta.window_shard_count, 1);

        // /params
        let params: pir_types::YpirScenario = client
            .get(format!("{base}/params"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(params.num_items, decryption_types::DECRYPT_DB_ROWS as u64);

        // /query — verify leaf round-trip
        let row_idx: u32 = 0;
        let response = client
            .post(format!("{base}/query"))
            .body(row_idx.to_le_bytes().to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = response.bytes().await.unwrap();
        assert_eq!(body.len(), decryption_types::DECRYPT_ROW_BYTES);

        let stored =
            DecryptionLeaf::from_bytes(&body[..decryption_types::DECRYPT_LEAF_BYTES]).unwrap();
        assert_eq!(stored, leaf);

        server.abort();
    }

    #[tokio::test]
    async fn query_returns_503_before_pir_ready() {
        let engine = Arc::new(StubPirEngine);
        let state = Arc::new(AppState::new(engine.clone()));

        let router = build_router(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });

        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        let response = client
            .post(format!("{base}/query"))
            .body(0u32.to_le_bytes().to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 503);

        let response = client.get(format!("{base}/metadata")).send().await.unwrap();
        assert_eq!(response.status(), 503);

        server.abort();
    }
}
