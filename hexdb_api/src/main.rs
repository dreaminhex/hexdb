use axum::{serve, Router};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use tokio::{net::TcpListener, sync::mpsc};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use hexdb_core::{init_logging, load_config, MemoryEngine, Wal, wal_writer_task, recover_from_wal};
use hexdb_api::routes::app_router;
use tracing::{info, warn, error};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging("hexdb");

    info!("🏎️ HexDB is starting...");

    let config = load_config().expect("❌ Failed to load configuration.");
    info!(?config, "✅ HexDB configuration loaded.");

    // Setup the WAL (Write-Ahead Log)
    if config.storage.wal_key.is_empty() {
        error!("❌ WAL key is not set. Please set the WAL key in the configuration.");
        std::process::exit(1);
    }

    let key_b64 = config.storage.wal_key.strip_prefix("base64:").unwrap();
    let key = STANDARD.decode(key_b64)
    .map_err(|e| anyhow::anyhow!("❌ Failed to decode WAL key: {}", e))?;

    let (wal_tx, wal_rx) = mpsc::channel::<Wal>(1024);
    let wal_path = PathBuf::from("./.hexdb.dat");

    // Setup the MemoryEngine
    let engine = Arc::new(MemoryEngine::new(wal_tx));

    // Begin recovering from the WAL if it exists
    recover_from_wal(wal_path.clone(), &key, engine.clone()).await?;

    // Start the WAL writer task
    let key = Arc::new(key);
    tokio::spawn(wal_writer_task(wal_rx, wal_path.clone(), key.clone()));

    // Start the HTTP server
    let app: Router = app_router(engine.clone());

    let addr: SocketAddr = config.network.engine_endpoint.parse().unwrap_or_else(|err| {
        error!(%err, "❌ Invalid endpoint: {}", config.network.engine_endpoint);
        std::process::exit(1);
    });

    let listener = TcpListener::bind(addr).await?;
    info!("⌬ HexDB is listening on http://{}", addr);
    serve(listener, app.into_make_service())
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = signal(SignalKind::interrupt()).expect("Failed to listen for SIGINT");
        let mut sigterm = signal(SignalKind::terminate()).expect("Failed to listen for SIGTERM");

        tokio::select! {
            _ = sigint.recv() => {
                warn!("🛑 Received SIGINT (Ctrl+C). Shutting down...");
            }
            _ = sigterm.recv() => {
                warn!("🛑 Received SIGTERM. Shutting down...");
            }
        }
    }

    #[cfg(windows)]
    {
        use tokio::signal::ctrl_c;

        if let Err(e) = ctrl_c().await {
            error!("❌ Failed to listen for Ctrl+C: {}", e);
        } else {
            warn!("🛑 Received Ctrl+C. Shutting down...");
        }
    }
}
