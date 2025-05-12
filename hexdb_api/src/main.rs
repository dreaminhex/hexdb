use axum::{serve, Router};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use tokio::{net::TcpListener, sync::mpsc};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use hexdb_core::{init_logging, load_config, recover_from_all_wal_files, wal_writer_task, MemoryEngine, Wal};
use hexdb_api::routes::app_router;
use tracing::{info, warn, error};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging("hexdb");

    info!("HexDB is starting...");

    let config = load_config().expect("❌ Failed to load configuration.");
    info!(?config, "✅ HexDB configuration loaded.");

    // Setup the WAL (Write-Ahead Log)
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

    // Setup the MemoryEngine
    let engine = Arc::new(MemoryEngine::new(wal_tx, config.clone()).await);

    info!("✅ Hex '{}' (id: {}) initialized.", engine.name.clone(), engine.id.clone());

    // Begin recovering from the wal files.
    info!("🥁 Recovering data from any write-ahead log files...");
    recover_from_all_wal_files(PathBuf::from("./.hexdb"), &key, engine.clone(), true).await?;

    info!("🥁 Recovering data from SSTables...");
    engine.load_sstables().await?;

    // Start the WAL writer task
    info!("🥁 Starting the write-ahead log task...");
    let key = Arc::new(key);
    tokio::spawn(wal_writer_task(config.clone(), wal_rx, wal_path.clone(), key.clone()));

    // Start the SST writer task
    info!("🥁 Starting the SST writer task...");
    let engine_flush = engine.clone();

    // Spawn a task to compact the SSTables
    let engine_compact = engine.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = engine_compact.compact_all().await {
                warn!("❌ Compaction error: {}", e);
            }
            tokio::time::sleep(Duration::from_secs(config.storage.compaction_frequency)).await;
        }
    });

    // Spawn a task to flush the WAL to SSTables
    tokio::spawn(async move {
        loop {
            if engine_flush.store.len() > engine_flush.adaptive_max_docs() {
                if let Err(e) = engine_flush.flush_to_sstable(&wal_tx_flush).await {
                    warn!("❌ Flush failed: {}", e);
                }
                wal_tx_flush.send(Wal::Rotate).await.ok();
            }
            tokio::time::sleep(Duration::from_secs(config.storage.wal_flush_check_frequency)).await;
        }
    });

    // Spawn a task to sweep expired documents (ttl)
    let engine_ttl = engine.clone();
    let sweep_minutes = config.memory.ttl_scan_frequency;

    tokio::spawn(async move {
        loop {
            if let Err(e) = engine_ttl.sweep_expired_documents().await {
                warn!("❌ TTL sweep failed: {}", e);
            }
            tokio::time::sleep(Duration::from_secs((sweep_minutes * 60) as u64)).await;
        }
    });

    // Start the HTTP server
    info!("🥁 Starting the HTTP server...");
    let app: Router = app_router(engine.clone());
    let addr: SocketAddr = config.network.engine_endpoint.parse().unwrap_or_else(|err| {
        error!(%err, "❌ Invalid endpoint: {}", config.network.engine_endpoint);
        std::process::exit(1);
    });

    let listener = TcpListener::bind(addr).await?;
    info!("🎉 HexDB is listening on http://{}", addr);
    serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = signal(SignalKind::interrupt()).expect("❌ Failed to listen for SIGINT!");
        let mut sigterm = signal(SignalKind::terminate()).expect("❌ Failed to listen for SIGTERM!");

        tokio::select! {
            _ = sigint.recv() => {
                warn!("🛑 Received SIGINT (Ctrl+C). Shutting down gracefully...");
            }
            _ = sigterm.recv() => {
                warn!("🛑 Received SIGTERM. Shutting down gracefully...");
            }
        }
    }

    #[cfg(windows)]
    {
        use tokio::signal::ctrl_c;

        if let Err(e) = ctrl_c().await {
            error!("❌ Failed to listen for Ctrl+C: {}", e);
        } else {
            warn!("🛑 Received Ctrl+C. Shutting down gracefully...");
        }
    }
}
