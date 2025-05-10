use axum::{serve, Router};
use tokio::net::TcpListener;
use std::{net::SocketAddr, sync::Arc};
use hexdb_core::{init_logging, load_config, MemoryEngine};
use hexdb_api::routes::app_router;
use tracing::{info, error};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging("hexdb");

    let config = load_config().expect("❌ Failed to load configuration.");
    info!(?config, "✔️ HexDB configuration loaded.");

    let engine = Arc::new(MemoryEngine::new());
    let app: Router = app_router(engine.clone());

    let addr: SocketAddr = config.engine_endpoint.parse().unwrap_or_else(|err| {
        error!(%err, "❌ Invalid endpoint: {}", config.engine_endpoint);
        std::process::exit(1);
    });

    let listener = TcpListener::bind(addr).await?;
    info!("🚀 HexDB is listening on http://{}", addr);
    serve(listener, app.into_make_service()).await?;

    Ok(())
}
