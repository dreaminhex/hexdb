use axum::{
    Router,
    routing::{get, post, put, patch, delete},
};
use std::sync::Arc;
use hexdb_core::memory_engine::MemoryEngine;
use crate::handlers::*;

pub fn app_router(engine: Arc<MemoryEngine>) -> Router {
    Router::new()
        // Document routes
        .route("/{tessellation}", post(insert_doc))
        .route("/{tessellation}", put(update_doc))
        .route("/{tessellation}", patch(patch_doc))
        .route("/{tessellation}/{id}", get(get_doc))
        .route("/{tessellation}/{id}", delete(delete_doc))

        // Tessellation routes
        .route("/tessellation/{name}", post(create_tessellation))
        .route("/tessellation/{name}", delete(delete_tessellation))

        // Utility
        .route("/{tessellation}/count", get(count_docs))

        // TODO: Health, status

        .with_state(engine)
}
