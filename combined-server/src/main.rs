use clap::Parser;
use pir_types::YpirScenario;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[cfg(feature = "nullifier")]
use spend_server::pir_ypir::YpirPirEngine as NfPirEngine;
#[cfg(feature = "nullifier")]
use spend_types::{BUCKET_BYTES, NUM_BUCKETS, TARGET_SIZE};

#[cfg(feature = "witness")]
use witness_server::pir_ypir::YpirPirEngine as WitPirEngine;
#[cfg(feature = "witness")]
use witness_types::{L0_DB_ROWS, SUBSHARD_ROW_BYTES};

#[cfg(feature = "decryption")]
use decryption_server::pir_ypir::YpirPirEngine as DecPirEngine;
#[cfg(feature = "decryption")]
use decryption_types::{DECRYPT_DB_ROWS, DECRYPT_ROW_BYTES};

#[derive(Parser)]
#[command(name = "spend-server", about = "Zcash PIR server")]
struct Cli {
    /// Directory for snapshots (creates nullifier/, witness/, decryption/ subdirectories)
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,

    /// lightwalletd gRPC endpoint(s), can be repeated
    #[arg(long, required = true)]
    lwd_url: Vec<String>,

    /// HTTP listen address
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: SocketAddr,

    /// Target nullifier count before eviction
    #[cfg(feature = "nullifier")]
    #[arg(long, default_value_t = TARGET_SIZE)]
    target_size: usize,

    /// Blocks between snapshots
    #[arg(long, default_value_t = 100)]
    snapshot_interval: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    #[cfg(feature = "nullifier")]
    std::fs::create_dir_all(cli.data_dir.join("nullifier"))?;
    #[cfg(feature = "witness")]
    std::fs::create_dir_all(cli.data_dir.join("witness"))?;
    #[cfg(feature = "decryption")]
    std::fs::create_dir_all(cli.data_dir.join("decryption"))?;

    let config = combined_server::server::CombinedConfig {
        #[cfg(feature = "nullifier")]
        target_size: cli.target_size,
        snapshot_interval: cli.snapshot_interval,
        data_dir: cli.data_dir,
        lwd_urls: cli.lwd_url,
        listen_addr: cli.listen,
    };

    let features: Vec<&str> = vec![
        #[cfg(feature = "nullifier")]
        "nullifier",
        #[cfg(feature = "witness")]
        "witness",
        #[cfg(feature = "decryption")]
        "decryption",
    ];

    tracing::info!(
        listen = %config.listen_addr,
        lwd_endpoints = ?config.lwd_urls,
        subsystems = ?features,
        data_dir = %config.data_dir.display(),
        "starting spend-server",
    );

    #[cfg(feature = "nullifier")]
    let nf_engine = {
        let nf_scenario = YpirScenario {
            num_items: NUM_BUCKETS as u64,
            item_size_bits: (BUCKET_BYTES * 8) as u64,
        };
        Arc::new(NfPirEngine::new(&nf_scenario))
    };

    #[cfg(feature = "witness")]
    let wit_engine = {
        let wit_scenario = YpirScenario {
            num_items: L0_DB_ROWS as u64,
            item_size_bits: (SUBSHARD_ROW_BYTES * 8) as u64,
        };
        Arc::new(WitPirEngine::new(&wit_scenario))
    };

    #[cfg(feature = "decryption")]
    let dec_engine = {
        let dec_scenario = YpirScenario {
            num_items: DECRYPT_DB_ROWS as u64,
            item_size_bits: (DECRYPT_ROW_BYTES * 8) as u64,
        };
        Arc::new(DecPirEngine::new(&dec_scenario))
    };

    combined_server::server::run(
        config,
        #[cfg(feature = "nullifier")]
        nf_engine,
        #[cfg(feature = "witness")]
        wit_engine,
        #[cfg(feature = "decryption")]
        dec_engine,
    )
    .await?;

    Ok(())
}
