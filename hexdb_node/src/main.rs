use axum::{
    extract::{Path, State},
    routing::{get, post, delete},
    http::StatusCode,
    response::IntoResponse,
    Json, Router, serve
};
use tokio::net::TcpListener;
use std::{net::SocketAddr, sync::Arc};
use hexdb_core::{init_logging, load_config, Document, Engine, MemoryEngine};
use tracing::{info, error};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging("hexdb_node");

    let config = load_config().expect("Failed to load configuration");
    info!(?config, "HexDB configuration loaded");

    // Shared engine instance
    let engine = Arc::new(MemoryEngine::new());

    // App routes
    let app = Router::new()
        .route("/health", get(|| async { "OK" }))
        .route("/doc/:id", get(get_doc).delete(delete_doc))
        .route("/doc", post(upsert_doc))
        .route("/count", get(count_docs))
        .with_state(engine.clone());

    let addr: SocketAddr = config.engine_endpoint.parse().unwrap_or_else(|err| {
        error!(%err, "Invalid engine_endpoint: {}", config.engine_endpoint);
        std::process::exit(1);
    });

    let listener = TcpListener::bind(addr).await?;
    info!("HexDB Node listening on http://{}", addr);
    serve(listener, app.into_make_service()).await?;

    Ok(())
}

// === API Handlers ===

async fn get_doc(Path(id): Path<String>, State(engine): State<Arc<MemoryEngine>>) -> Json<Option<Document>> {
    match engine.get(&id).await {
        Ok(doc) => Json(doc),
        Err(_) => Json(None),
    }
}

async fn upsert_doc(State(engine): State<Arc<MemoryEngine>>, Json(doc): Json<Document>) -> &'static str {
    if engine.upsert(doc).await.is_ok() {
        "OK"
    } else {
        "FAIL"
    }
}

async fn delete_doc(Path(id): Path<String>, State(engine): State<Arc<MemoryEngine>>) -> &'static str {
    if engine.delete(&id).await.is_ok() {
        "OK"
    } else {
        "FAIL"
    }
}

async fn count_docs(State(engine): State<Arc<MemoryEngine>>) -> String {
    engine.count().await.map(|c| c.to_string()).unwrap_or("0".into())
}
