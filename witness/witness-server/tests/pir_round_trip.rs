#![cfg(feature = "ypir")]

//! End-to-end PIR round-trip test against mainnet (Test B).
//!
//! Syncs from lightwalletd, builds a YPIR database, starts the witness
//! server in-process, and uses [`WitnessClient`] to issue real encrypted
//! queries over HTTP. Verifies that the reconstructed witness root matches
//! the tree root computed from ingested commitments.
//!
//! Validates the full pipeline: chain sync, PIR DB serialization, YPIR
//! encrypt/decrypt, HTTP transport, client reconstruction, and
//! leaf-to-root hash verification.
//!
//! Requires network access to `zec.rocks:443`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;
use witness_client::WitnessClient;
use witness_server::pir_ypir::YpirPirEngine;
use witness_server::server::{build_router, run_sync_only};
use witness_server::state::ServerConfig;
use witness_types::*;

const LWD_ENDPOINT: &str = "https://zec.rocks:443";

fn data_dir() -> (PathBuf, Option<tempfile::TempDir>) {
    if let Ok(dir) = std::env::var("PIR_TEST_DATA_DIR") {
        let path = PathBuf::from(&dir);
        std::fs::create_dir_all(&path).expect("failed to create PIR_TEST_DATA_DIR");
        (path, None)
    } else {
        let tmp = tempfile::tempdir().expect("failed to create tempdir");
        let path = tmp.path().to_path_buf();
        (path, Some(tmp))
    }
}

#[tokio::test]
async fn full_pir_round_trip() {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init()
        .ok();

    // ── 1. Sync from mainnet and build YPIR database ──

    let (dir, _tmp_guard) = data_dir();

    let config = ServerConfig {
        snapshot_interval: 0,
        data_dir: dir,
        lwd_urls: vec![LWD_ENDPOINT.to_string()],
        listen_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
    };

    let scenario = pir_types::YpirScenario {
        num_items: L0_DB_ROWS as u64,
        item_size_bits: (SUBSHARD_ROW_BYTES * 8) as u64,
    };
    let engine = Arc::new(YpirPirEngine::new(&scenario));

    tracing::info!("starting sync and YPIR setup");

    let (app_state, tree) = run_sync_only(config, engine)
        .await
        .expect("run_sync_only failed");

    let tree_root = tree.tree_root();
    let tree_size = tree.tree_size();
    let anchor_height = tree.latest_height().expect("tree has no blocks");

    tracing::info!(
        tree_size,
        anchor_height,
        tree_root = hex::encode(tree_root),
        "sync complete, YPIR database built"
    );

    // ── 2. Start in-process HTTP server ──

    let router = build_router(app_state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind listener");
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let base_url = format!("http://{addr}");
    tracing::info!(url = %base_url, "server listening");

    // ── 3. Connect WitnessClient ──

    let client = WitnessClient::connect(&base_url)
        .await
        .expect("WitnessClient::connect failed");

    assert_eq!(client.anchor_height(), anchor_height);
    tracing::info!("client connected, broadcast cached");

    // ── 4. Query witnesses for multiple positions ──

    let window_start = tree.window_start_shard();
    let window_start_leaf = (window_start as u64) * (SHARD_LEAVES as u64);

    // Position A: midpoint of the window (completed shard region)
    let pos_a = window_start_leaf + (tree_size - window_start_leaf) / 4;

    // Position B: near the frontier (partially-filled subshard)
    let pos_b = tree.leaf_offset() + tree.leaves().len() as u64 - 100;

    for (label, position) in [("completed-region", pos_a), ("frontier", pos_b)] {
        let (shard_idx, subshard_idx, leaf_idx) = decompose_position(position);
        tracing::info!(
            label,
            position,
            shard_idx,
            subshard_idx,
            leaf_idx,
            "querying witness"
        );

        let witness = client
            .get_witness(position)
            .await
            .unwrap_or_else(|e| panic!("get_witness({position}) failed: {e}"));

        assert_eq!(
            witness.position, position,
            "witness position mismatch for {label}"
        );
        assert_eq!(
            witness.anchor_height, anchor_height,
            "anchor height mismatch for {label}"
        );
        assert_eq!(
            witness.anchor_root, tree_root,
            "\nwitness root mismatch for {label}!\n  \
             witness:  {}\n  tree:     {}\n\n\
             The YPIR round-trip corrupted or misrouted row data.",
            hex::encode(witness.anchor_root),
            hex::encode(tree_root)
        );

        tracing::info!(
            label,
            position,
            root = hex::encode(witness.anchor_root),
            "PASS: witness root matches tree root"
        );
    }

    tracing::info!(
        "PASS: full PIR round-trip verified — \
         YPIR encrypt/decrypt preserves correct witness data"
    );
}
