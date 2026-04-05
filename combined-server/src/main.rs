use clap::Parser;
use combined_server::server;
use pir_types::YpirScenario;
use spend_server::pir_ypir::YpirPirEngine as NfPirEngine;
use spend_types::{BUCKET_BYTES, NUM_BUCKETS, TARGET_SIZE};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use witness_server::pir_ypir::YpirPirEngine as WitPirEngine;
use witness_types::{L0_DB_ROWS, SUBSHARD_ROW_BYTES};

#[derive(Parser)]
#[command(name = "pir-server", about = "Combined nullifier + witness PIR server")]
struct Cli {
    /// Directory for snapshots (creates nullifier/ and witness/ subdirectories)
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,

    /// lightwalletd gRPC endpoint(s), can be repeated
    #[arg(long, required = true)]
    lwd_url: Vec<String>,

    /// HTTP listen address
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: SocketAddr,

    /// Target nullifier count before eviction
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

    std::fs::create_dir_all(cli.data_dir.join("nullifier"))?;
    std::fs::create_dir_all(cli.data_dir.join("witness"))?;

    let config = server::CombinedConfig {
        target_size: cli.target_size,
        snapshot_interval: cli.snapshot_interval,
        data_dir: cli.data_dir,
        lwd_urls: cli.lwd_url,
        listen_addr: cli.listen,
    };

    tracing::info!(
        listen = %config.listen_addr,
        lwd_endpoints = ?config.lwd_urls,
        target_size = config.target_size,
        data_dir = %config.data_dir.display(),
        "starting combined pir-server",
    );

    let nf_scenario = YpirScenario {
        num_items: NUM_BUCKETS as u64,
        item_size_bits: (BUCKET_BYTES * 8) as u64,
    };
    let wit_scenario = YpirScenario {
        num_items: L0_DB_ROWS as u64,
        item_size_bits: (SUBSHARD_ROW_BYTES * 8) as u64,
    };

    let nf_engine = Arc::new(NfPirEngine::new(&nf_scenario));
    let wit_engine = Arc::new(WitPirEngine::new(&wit_scenario));

    server::run(config, nf_engine, wit_engine).await?;

    Ok(())
}
