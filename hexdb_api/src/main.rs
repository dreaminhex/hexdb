use axum::{
    extract::{Path, State}, 
    response::IntoResponse, 
    routing::{get, post, delete}, serve, Json, Router,
    http::StatusCode,
};
use tokio::net::TcpListener;
use std::{net::SocketAddr, sync::Arc};
use hexdb_core::{init_logging, load_config, Document, Engine, MemoryEngine};
use tracing::{info, error};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging("hexdb");

    let config = load_config().expect("❌ Failed to load configuration.");
    info!(?config, "✔️ HexDB configuration loaded.");

    // Shared engine instance
    let engine = Arc::new(MemoryEngine::new());

    // App routes
    let app = Router::new()
        .route("/health", get(|| async { "✔️ Node is healthy." }))
        .route("/api/{id}", get(get_doc).delete(delete_doc))
        .route("/tessellation/{name}", post(create_tessellation))
        .route("/tessellation/{name}", delete(delete_tessellation))
        .route("/api", post(upsert_doc))
        .route("/count", get(count_docs))
        .with_state(engine.clone());

    let addr: SocketAddr = config.engine_endpoint.parse().unwrap_or_else(|err| {
        error!(%err, "❌ Invalid endpoint: {}", config.engine_endpoint);
        std::process::exit(1);
    });

    let listener = TcpListener::bind(addr).await?;
    info!("🚀 HexDB is listening on http://{}", addr);
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
        "✔️ Ok."
    } else {
        "❌ Error."
    }
}

async fn delete_doc(Path(id): Path<String>, State(engine): State<Arc<MemoryEngine>>) -> &'static str {
    if engine.delete(&id).await.is_ok() {
        "✔️ Ok."
    } else {
        "❌ Error."
    }
}

async fn count_docs(State(engine): State<Arc<MemoryEngine>>) -> String {
    engine.count().await.map(|c| c.to_string()).unwrap_or("0".into())
}

pub async fn create_tessellation(
    Path(name): Path<String>,
    State(engine): State<Arc<MemoryEngine>>,
) -> impl IntoResponse {
    let mut node = engine.node.lock().await;
    if node.create_tessellation(&name) {
        (StatusCode::OK, format!("✔️ Created tessellation '{}'", name))
    } else {
        (StatusCode::CONFLICT, format!("⚠️ Tessellation '{}' already exists", name))
    }
}

pub async fn delete_tessellation(
    Path(name): Path<String>,
    State(engine): State<Arc<MemoryEngine>>,
) -> impl IntoResponse {
    let mut node = engine.node.lock().await;
    if node.drop_tessellation(&name) {
        (StatusCode::OK, format!("🗑️ Deleted tessellation '{}'", name))
    } else {
        (StatusCode::NOT_FOUND, format!("⚠️ Tessellation '{}' not found", name))
    }
}