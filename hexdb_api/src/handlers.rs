use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use std::sync::Arc;
use hexdb_core::{memory_engine::MemoryEngine, engine::Engine, document::Document};
use serde_json::Value;

pub async fn get_doc(
    Path((tess, id)): Path<(String, String)>,
    State(engine): State<Arc<MemoryEngine>>
) -> Json<Option<Document>> {
    match engine.get_document(&tess, &id).await {
        Ok(doc) => Json(doc),
        Err(_) => Json(None),
    }
}

pub async fn insert_doc(Path(tess): Path<String>, State(engine): State<Arc<MemoryEngine>>, Json(json): Json<Value>) -> impl IntoResponse {
    match engine.insert_json(&tess, json).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

pub async fn update_doc(Path(tess): Path<String>, State(engine): State<Arc<MemoryEngine>>, Json(json): Json<Value>) -> impl IntoResponse {
    match engine.update_json(&tess, json).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

pub async fn patch_doc(Path(tess): Path<String>, State(engine): State<Arc<MemoryEngine>>, Json(json): Json<Value>) -> impl IntoResponse {
    match engine.patch_json(&tess, json).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

pub async fn delete_doc(Path((tess, id)): Path<(String, String)>, State(engine): State<Arc<MemoryEngine>>) -> impl IntoResponse {
    match engine.delete_document(&tess, &id).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::NOT_FOUND,
    }
}

pub async fn count_docs(Path(tess): Path<String>, State(engine): State<Arc<MemoryEngine>>) -> String {
    engine.count_documents(&tess).await.map(|c| c.to_string()).unwrap_or("0".into())
}

pub async fn create_tessellation(Path(name): Path<String>, State(engine): State<Arc<MemoryEngine>>) -> impl IntoResponse {
    let mut node = engine.node.lock().await;
    if node.create_tessellation(&name) {
        (StatusCode::OK, format!("✔️ Created tessellation '{}'", name))
    } else {
        (StatusCode::CONFLICT, format!("⚠️ Tessellation '{}' already exists", name))
    }
}

pub async fn delete_tessellation(Path(name): Path<String>, State(engine): State<Arc<MemoryEngine>>) -> impl IntoResponse {
    let mut node = engine.node.lock().await;
    if node.drop_tessellation(&name) {
        (StatusCode::OK, format!("🗑️ Deleted tessellation '{}'", name))
    } else {
        (StatusCode::NOT_FOUND, format!("⚠️ Tessellation '{}' not found", name))
    }
}
