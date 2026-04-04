use clap::Parser;
use pir_types::YpirScenario;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use witness_server::pir_ypir::YpirPirEngine;
use witness_server::server;
use witness_server::state::ServerConfig;
use witness_types::{L0_DB_ROWS, SUBSHARD_ROW_BYTES};

#[derive(Parser)]
#[command(
    name = "witness-server",
    about = "Private note commitment witness server using YPIR"
)]
struct Cli {
    /// Directory for snapshots
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,

    /// lightwalletd gRPC endpoint(s), can be repeated
    #[arg(long, required = true)]
    lwd_url: Vec<String>,

    /// HTTP listen address
    #[arg(long, default_value = "0.0.0.0:8081")]
    listen: SocketAddr,

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

    std::fs::create_dir_all(&cli.data_dir)?;

    let config = ServerConfig {
        snapshot_interval: cli.snapshot_interval,
        data_dir: cli.data_dir,
        lwd_urls: cli.lwd_url,
        listen_addr: cli.listen,
    };

    tracing::info!(
        listen = %config.listen_addr,
        lwd_endpoints = ?config.lwd_urls,
        data_dir = %config.data_dir.display(),
        "starting witness-server",
    );

    let scenario = YpirScenario {
        num_items: L0_DB_ROWS as u64,
        item_size_bits: (SUBSHARD_ROW_BYTES * 8) as u64,
    };
    let engine = Arc::new(YpirPirEngine::new(&scenario));

    server::run(config, engine).await?;

    Ok(())
}
