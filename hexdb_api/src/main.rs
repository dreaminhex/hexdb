/// HexDB API Module
/// This module provides the main entry point for the HexDB API server.
/// It initializes the server, sets up the necessary components, and starts
/// the server to listen for incoming requests.

use axum::{serve, Router};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use tokio::{net::TcpListener, sync::mpsc};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use hexdb_core::{
    init_logging, 
    load_config, 
    recover_from_wal, 
    spawn_compaction_task, 
    spawn_flush_task, 
    spawn_ttl_sweep_task, 
    spawn_vertex_monitoring_task, 
    spawn_wal_writer_task, 
    HexDBEngine, 
    Wal};
use hexdb_api::{routes::app_router, init::init_security};
use tracing::{info, warn, error};
use tokio::sync::watch;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    init_logging("hexdb");

    info!("▶️  HexDB is starting...");

    let config = load_config().expect("❌ Failed to load configuration.");
    info!(?config, "✅ HexDB configuration loaded.");

    // Ensure the WAL encryption key is set and that it can be decoded.
    if config.storage.encryption_key.is_empty() {
        error!("❌ WAL key is not set. Please set the WAL key in the configuration.");
        std::process::exit(1);
    }

    let key_b64 = config.storage.encryption_key.strip_prefix("base64:").unwrap();
    let key = STANDARD.decode(key_b64)
    .map_err(|e| anyhow::anyhow!("❌ Failed to decode WAL key: {}", e))?;

    let (wal_tx, wal_rx) = mpsc::channel::<Wal>(1024);
    let wal_tx_flush = wal_tx.clone();
    let wal_path = PathBuf::from("./.hexdb/.hexdb.dat");

    // Setup the HexDBEngine
    let engine = Arc::new(HexDBEngine::new(wal_tx, config.clone()).await);
    info!("✅ Hex '{}' (id: {}) initialized.", engine.name.clone(), engine.id.clone());

    // Begin recovering from the WAL file(s).
    info!("🛠️  Recovering data from any write-ahead log files...");
    recover_from_wal(PathBuf::from("./.hexdb"), &key, engine.clone(), true).await?;

    // Recover from any existing SSTables.
    info!("🛠️  Recovering data from SSTables...");
    engine.sst.read_all(&engine.node).await?;

    // Initialize security settings & create defaults if not present.
    info!("🔐 Initializing security settings...");
    init_security(engine.clone()).await?;

    // Create channels for shutdown signals
    let (shutdown_tx, mut shutdown_rx) = watch::channel(());

    // Start the WAL writer task
    info!("🏃‍➡️  Starting the write-ahead log task (1 of 5)...");
    let key = Arc::new(key);
    spawn_wal_writer_task(config.clone(), wal_rx, wal_path, key);

    // Start the SST compaction task
    info!("🏃‍➡️  Starting the SST compaction task (2 of 5)...");
    spawn_compaction_task(engine.clone(), Duration::from_secs(config.storage.compaction_frequency), shutdown_rx.clone());

    // Spawn a task to flush the WAL to SSTables
    info!("🏃‍➡️  Starting the WAL flush task (3 of 5)...");
    spawn_flush_task(engine.clone(), wal_tx_flush, Duration::from_secs(config.storage.wal_flush_check_frequency), shutdown_rx.clone());

    // Spawn a task to sweep expired documents (ttl)
    info!("🏃‍➡️  Starting the TTL sweep task (4 of 5)...");
    let ttl_interval = Duration::from_secs(config.memory.ttl_scan_frequency * 60);
    spawn_ttl_sweep_task(engine.clone(), ttl_interval, shutdown_rx.clone());

    // Spawn a task to monitor vertices
    info!("🏃‍➡️  Starting the vertex monitoring task (5 of 5)...");
    spawn_vertex_monitoring_task(engine.clone(), Duration::from_secs(config.memory.vertex_integrity_check_frequency), shutdown_rx.clone());

    // Start the HTTP server
    info!("🌐 Starting the HTTP server...");
    let app: Router = app_router(engine.clone());
    let addr: SocketAddr = config.network.api_endpoint.parse().unwrap_or_else(|err| {
        error!(%err, "❌ Invalid endpoint: {}.", config.network.api_endpoint);
        std::process::exit(1);
    });
        
    // Spawn a task to listen for shutdown and notify all tasks.
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(());
    });

    // Bind the server to the specified address and port.
    let listener = TcpListener::bind(addr).await?;
    info!("💽  HexDB API is listening at http://{}", addr);
    serve(listener, app.into_make_service())
        .with_graceful_shutdown(async move {     
            let _ = shutdown_rx.changed().await;
        })
        .await?;
    
    Ok(())
}

async fn shutdown_signal() {


    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = signal(SignalKind::interrupt()).expect("❌ Failed to capture SIGINT. Shutdown can still occur, but it could result in data loss.");
        let mut sigterm = signal(SignalKind::terminate()).expect("❌ Failed to capture SIGTERM. Shutdown can still occur, but it could result in data loss.");

        tokio::select! {
            _ = sigint.recv() => {
                warn!("🛑 Received SIGINT (Ctrl+C)...");
            }
            _ = sigterm.recv() => {
                warn!("🛑 Received SIGTERM...");
            }
        }
    }

    #[cfg(windows)]
    {
        use tokio::signal::ctrl_c;

        if let Err(e) = ctrl_c().await {
            error!("❌ Failed to capture shutdown (Ctrl+C): {}. Shutdown can still occur, but it could result in data loss.", e);
        } else {
            warn!("🛑 Received Ctrl+C...");
        }
    }

    info!("🛑 HexDB is shutting down gracefully...");

}
