use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use tracing::{error, info, warn};
use std::sync::Arc;
use hexdb_core::{constant_time_eq, engine::HexDBEngine, metrics::{collect, HexMeta}, EngineError, SHUTDOWN_TOKEN_HEADER};
use serde::Deserialize;
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
/// Optional query parameters for writes.
#[derive(Debug, Default, Deserialize)]
pub struct WriteParams {
    /// Time to live in seconds. The document expires this long after the write.
    pub ttl: Option<u64>,
}

impl WriteParams {
    /// The expiry time in epoch milliseconds, if a TTL was given.
    fn expiry(&self) -> Option<i64> {
        self.ttl
            .map(|secs| Utc::now().timestamp_millis().saturating_add((secs.min(i64::MAX as u64 / 1000) * 1000) as i64))
    }
}

/// Map an engine error to an HTTP status and log unexpected failures.
fn error_status(context: &str, e: &anyhow::Error) -> (StatusCode, String) {
    match e.downcast_ref::<EngineError>() {
        Some(EngineError::NotFound(m)) => (StatusCode::NOT_FOUND, m.clone()),
        Some(EngineError::Invalid(m)) => (StatusCode::BAD_REQUEST, m.clone()),
        Some(EngineError::Conflict(m)) => (StatusCode::CONFLICT, m.clone()),
        None => {
            error!("❌ {}: {:#}", context, e);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{}.", context))
        }
    }
}

/// Get a document by tessellation and ID.
/// Returns the document if found, or None if not found.
pub async fn get_doc(
    Path((tess, id)): Path<(String, String)>,
    State(engine): State<Arc<HexDBEngine>>,
) -> Response {
    match engine.get_document(&tess, &id).await {
        Ok(Some(doc)) => Json(Some(doc)).into_response(),
        Ok(None) => {
            warn!("⚠️ Document not found: {}/{}", tess, id);
            Json(None::<()>).into_response()
        }
        Err(e) => error_status("Failed to fetch document", &e).into_response(),
    }
}

/// Insert a new document into the tessellation.
pub async fn insert_doc(
    Path(tess): Path<String>,
    Query(params): Query<WriteParams>,
    State(engine): State<Arc<HexDBEngine>>,
    Json(json): Json<Value>,
) -> Response {
    match engine.insert_json(&tess, json, params.expiry()).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => error_status("Failed to insert document", &e).into_response(),
    }
}

/// Update an existing document in the tessellation.
pub async fn update_doc(
    Path(tess): Path<String>,
    Query(params): Query<WriteParams>,
    State(engine): State<Arc<HexDBEngine>>,
    Json(json): Json<Value>,
) -> Response {
    match engine.update_json(&tess, json, params.expiry()).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => error_status("Failed to update document", &e).into_response(),
    }
}

/// Patch an existing document in the tessellation.
pub async fn patch_doc(
    Path(tess): Path<String>,
    Query(params): Query<WriteParams>,
    State(engine): State<Arc<HexDBEngine>>,
    Json(json): Json<Value>,
) -> Response {
    match engine.patch_json(&tess, json, params.expiry()).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => error_status("Failed to patch document", &e).into_response(),
    }
}

/// Delete a document from the tessellation by ID.
pub async fn delete_doc(Path((tess, id)): Path<(String, String)>, State(engine): State<Arc<HexDBEngine>>) -> Response {
    match engine.delete_document(&tess, &id).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => error_status("Failed to delete document", &e).into_response(),
    }
}

/// Count the number of documents in a tessellation.
pub async fn count_docs(Path(tess): Path<String>, State(engine): State<Arc<HexDBEngine>>) -> Response {
    match engine.count_documents(&tess).await {
        Ok(count) => count.to_string().into_response(),
        Err(e) => error_status("Failed to count documents", &e).into_response(),
    }
}

pub async fn create_tessellation(Path((tess_name, tess_type)): Path<(String, String)>, State(engine): State<Arc<HexDBEngine>>) -> Response {
    match engine.create_tessellation(&tess_name, &tess_type) {
        Ok(true) => (StatusCode::OK, format!("✅ Created tessellation '{}'.", tess_name)).into_response(),
        Ok(false) => (StatusCode::CONFLICT, format!("❗ Tessellation '{}' already exists.", tess_name)).into_response(),
        Err(e) => error_status("Failed to create tessellation", &e).into_response(),
    }
}

/// Delete a tessellation by name, with all of its documents.
pub async fn delete_tessellation(Path(name): Path<String>, State(engine): State<Arc<HexDBEngine>>) -> Response {
    match engine.delete_tessellation(&name).await {
        Ok(true) => (StatusCode::OK, format!("🗑️ Deleted tessellation '{}'.", name)).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, format!("❗ Tessellation '{}' not found.", name)).into_response(),
        Err(e) => error_status("Failed to delete tessellation", &e).into_response(),
    }
}

/// Flush unflushed writes to SSTables.
pub async fn flush(State(engine): State<Arc<HexDBEngine>>) -> Response {
    match engine.flush().await {
        Ok(stats) => Json(json!({
            "entries": stats.entries,
            "tessellations": stats.tessellations,
            "wal_segments_deleted": stats.wal_segments_deleted,
        }))
        .into_response(),
        Err(e) => error_status("Flush failed", &e).into_response(),
    }
}

pub async fn status(State(engine): State<Arc<HexDBEngine>>) -> Json<HexMeta> {
    Json(collect(&engine).await)
}
