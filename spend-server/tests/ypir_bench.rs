#![cfg(feature = "ypir")]

use spend_server::pir_ypir::YpirPirEngine;
use spend_types::{
    hash_to_bucket, PirEngine, YpirScenario, BUCKET_BYTES, ENTRY_BYTES, NUM_BUCKETS,
};
use std::time::Instant;
use ypir::client::YPIRClient;
use ypir::serialize::ToBytes;

fn scenario() -> YpirScenario {
    YpirScenario {
        num_items: NUM_BUCKETS as u64,
        item_size_bits: (BUCKET_BYTES * 8) as u64,
    }
}

fn make_nf(seed: u32) -> [u8; 32] {
    let mut nf = [0u8; 32];
    nf[0..4].copy_from_slice(&seed.to_le_bytes());
    for i in 4..32 {
        nf[i] = ((seed >> ((i % 4) * 8)) as u8).wrapping_add(i as u8);
    }
    nf
}

#[test]
fn bench_ypir_performance() {
    let sc = scenario();
    println!("\n=== YPIR Performance Benchmark ===");
    println!(
        "DB config: {} buckets x {} bytes/bucket = {} MB total",
        NUM_BUCKETS,
        BUCKET_BYTES,
        NUM_BUCKETS * BUCKET_BYTES / (1024 * 1024),
    );

    // Build a realistic DB with scattered nullifiers
    let mut db_bytes = vec![0u8; NUM_BUCKETS * BUCKET_BYTES];
    let num_nfs = 1000;
    let mut nfs = Vec::with_capacity(num_nfs);
    for i in 0..num_nfs {
        let nf = make_nf(i as u32 * 7 + 1);
        let bucket_idx = hash_to_bucket(&nf) as usize;
        let offset = bucket_idx * BUCKET_BYTES;
        // Find first free slot in bucket
        for slot in 0..112 {
            let slot_offset = offset + slot * ENTRY_BYTES;
            if db_bytes[slot_offset..slot_offset + ENTRY_BYTES] == [0u8; 32] {
                db_bytes[slot_offset..slot_offset + ENTRY_BYTES].copy_from_slice(&nf);
                nfs.push(nf);
                break;
            }
        }
    }
    println!("Inserted {} nullifiers into DB", nfs.len());

    // Measure engine construction (params computation)
    let t0 = Instant::now();
    let engine = YpirPirEngine::new(&sc);
    let params_time = t0.elapsed();
    println!("Params computation: {:?}", params_time);

    // Measure setup (server construction + offline precomputation)
    let t0 = Instant::now();
    let state = engine.setup(&db_bytes, &sc).unwrap();
    let setup_time = t0.elapsed();
    println!("Setup (server + offline precomp): {:?}", setup_time);

    // Measure per-query online time (server side only)
    let params = engine.params();
    let client = YPIRClient::new(params);

    let bucket_idx = hash_to_bucket(&nfs[0]) as usize;
    let (query, seed) = client.generate_query_simplepir(bucket_idx);
    let query_bytes = query.to_bytes();
    println!("Query size: {} bytes", query_bytes.len());

    // Warmup
    let _ = engine.answer_query(&state, &query_bytes).unwrap();

    // Measure 5 trials
    let trials = 5;
    let mut server_times = Vec::new();
    for _ in 0..trials {
        let t0 = Instant::now();
        let response = engine.answer_query(&state, &query_bytes).unwrap();
        let elapsed = t0.elapsed();
        server_times.push(elapsed);
        assert!(!response.is_empty());
    }
    let avg_server_ms =
        server_times.iter().map(|d| d.as_millis()).sum::<u128>() as f64 / trials as f64;
    println!(
        "Server online time (avg of {}): {:.1} ms",
        trials, avg_server_ms,
    );
    for (i, t) in server_times.iter().enumerate() {
        println!("  trial {}: {:?}", i + 1, t);
    }

    // Measure response size
    let response = engine.answer_query(&state, &query_bytes).unwrap();
    println!("Response size: {} bytes", response.len());

    // Measure client decode time
    let t0 = Instant::now();
    let decoded = client.decode_response_simplepir(seed, &response);
    let decode_time = t0.elapsed();
    println!("Client decode time: {:?}", decode_time);
    println!("Decoded row size: {} bytes", decoded.len());

    // Verify correctness
    let bucket_data = &decoded[..BUCKET_BYTES];
    let found = bucket_data
        .chunks_exact(ENTRY_BYTES)
        .any(|chunk| chunk == nfs[0].as_slice());
    assert!(found, "benchmark correctness check failed");

    // Measure query generation time
    let t0 = Instant::now();
    let _ = client.generate_query_simplepir(0);
    let query_gen_time = t0.elapsed();
    println!("Client query generation: {:?}", query_gen_time);

    // Measure a second setup (simulates per-block PIR rebuild)
    let t0 = Instant::now();
    let _state2 = engine.setup(&db_bytes, &sc).unwrap();
    let rebuild_time = t0.elapsed();
    println!("PIR rebuild (re-setup): {:?}", rebuild_time);

    println!("\n=== Summary ===");
    println!("Setup:          {:?}", setup_time);
    println!("Rebuild:        {:?}", rebuild_time);
    println!("Server online:  {:.1} ms avg", avg_server_ms);
    println!("Client decode:  {:?}", decode_time);
    println!("Query gen:      {:?}", query_gen_time);
    println!("Query upload:   {} bytes", query_bytes.len());
    println!("Response:       {} bytes", response.len());

    // Feasibility checks
    let rebuild_s = rebuild_time.as_secs_f64();
    assert!(
        rebuild_s < 75.0,
        "PIR rebuild ({:.1}s) exceeds 75s block interval",
        rebuild_s,
    );
    println!(
        "\nFeasibility: rebuild {:.1}s < 75s block interval -> OK",
        rebuild_s,
    );
}
