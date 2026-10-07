use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use tracing::{error, info, warn};
use std::sync::Arc;
use hexdb_core::{constant_time_eq, engine::HexDBEngine, metrics::{collect, HexMeta}, SHUTDOWN_TOKEN_HEADER};
use serde_json::{json, Value};
use tokio::sync::watch;
use chrono::Utc;

/// Lets the shutdown endpoint trigger the server's graceful shutdown.
/// The token is generated at startup and written to the runtime file in the data directory.
#[derive(Clone)]
pub struct ShutdownHandle {
    pub token: Arc<String>,
    pub trigger: Arc<watch::Sender<()>>,
}

/// Liveness check. Does not lock the storage engine.
pub async fn health(State(engine): State<Arc<HexDBEngine>>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "id": engine.id.to_string(),
        "name": engine.name,
        "hex_type": engine.hex_type,
        "version": engine.version,
        "uptime_seconds": (Utc::now() - engine.start_datetime).num_seconds().max(0),
    }))
}

/// Begin a graceful shutdown. Requires the token from the runtime file in the `x-hexdb-shutdown-token` header.
pub async fn shutdown(Extension(handle): Extension<ShutdownHandle>, headers: HeaderMap) -> StatusCode {
    let presented = headers
        .get(SHUTDOWN_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if presented.is_empty() || !constant_time_eq(presented.as_bytes(), handle.token.as_bytes()) {
        warn!("⚠️ Rejected shutdown request with a missing or invalid token.");
        return StatusCode::UNAUTHORIZED;
    }

    info!("🛑 Shutdown requested through the API...");
    let _ = handle.trigger.send(());
    StatusCode::ACCEPTED
}
/// Get a document by tessellation and ID.
/// Returns the document if found, or None if not found.
pub async fn get_doc(
    Path((tess, id)): Path<(String, String)>,
    State(engine): State<Arc<HexDBEngine>>,
) -> impl IntoResponse {
    match engine.get_document(&tess, &id).await {
        Ok(Some(doc)) => {
            Json(Some(doc))
        }
        Ok(None) => {
            warn!("⚠️ Document not found: {}/{}", tess, id);
            Json(None)
        }
        Err(e) => {
            error!("❌ Failed to fetch document: {}", e);
            Json(None)
        }
    }
}

/// Insert a new document into the tessellation.
pub async fn insert_doc(Path(tess): Path<String>, State(engine): State<Arc<HexDBEngine>>, Json(json): Json<Value>) -> impl IntoResponse {
    match engine.insert_json(&tess, json).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

/// Update an existing document in the tessellation.
pub async fn update_doc(Path(tess): Path<String>, State(engine): State<Arc<HexDBEngine>>, Json(json): Json<Value>) -> impl IntoResponse {
    match engine.update_json(&tess, json).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

/// Patch an existing document in the tessellation.
pub async fn patch_doc(Path(tess): Path<String>, State(engine): State<Arc<HexDBEngine>>, Json(json): Json<Value>) -> impl IntoResponse {
    match engine.patch_json(&tess, json).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

/// Delete a document from the tessellation by ID.
pub async fn delete_doc(Path((tess, id)): Path<(String, String)>, State(engine): State<Arc<HexDBEngine>>) -> impl IntoResponse {
    match engine.delete_document(&tess, &id).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::NOT_FOUND,
    }
}

/// Count the number of documents in a tessellation.
pub async fn count_docs(Path(tess): Path<String>, State(engine): State<Arc<HexDBEngine>>) -> String {
    engine.count_documents(&tess).await.map(|c| c.to_string()).unwrap_or("0".into())
}

pub async fn create_tessellation(Path((tess_name, tess_type)): Path<(String, String)>, State(engine): State<Arc<HexDBEngine>>) -> impl IntoResponse {
    let mut node = engine.node.lock().await;
    if node.create_tessellation(&tess_name, &tess_type) {
        (StatusCode::OK, format!("✅ Created tessellation '{}'.", tess_name))
    } else {
        (StatusCode::CONFLICT, format!("❗ Tessellation '{}' already exists.", tess_name))
    }
}

/// Delete a tessellation by name.
/// This will also remove all associated documents from the vertices.
pub async fn delete_tessellation(Path(name): Path<String>, State(engine): State<Arc<HexDBEngine>>) -> impl IntoResponse {
    let mut node = engine.node.lock().await;
    if node.delete_tessellation(&name) {
        (StatusCode::OK, format!("🗑️ Deleted tessellation '{}'.", name))
    } else {
        (StatusCode::NOT_FOUND, format!("❗ Tessellation '{}' not found.", name))
    }
}

/// Flush the WAL to SSTable.
pub async fn flush(State(engine): State<Arc<HexDBEngine>>) -> impl IntoResponse {
    engine.sst.flush(engine.node.clone(), &engine.wal_tx).await;
}

pub async fn status(State(engine): State<Arc<HexDBEngine>>) -> Json<HexMeta> {
    Json(collect(&engine).await)
}
