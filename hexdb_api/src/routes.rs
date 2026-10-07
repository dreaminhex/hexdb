use axum::{
    extract::{DefaultBodyLimit, Request},
    http::Method,
    middleware::{self, Next},
    response::{Redirect, Response},
    routing::{get, post},
    Extension, Router,
};
use std::{path::PathBuf, sync::Arc};
use hexdb_core::engine::HexDBEngine;
use hexdb_query::HexDBSchema;
use tower_http::services::{ServeDir, ServeFile};
use crate::handlers::*;

/// URL prefix the admin UI is served under. Must match `base` in hexdb_admin/vite.config.ts.
pub const UI_PREFIX: &str = "/ui";
const UI_ROOT: &str = "/ui/";

/// Request body limit for bulk endpoints (other endpoints use axum's 2 MB default).
pub const BULK_BODY_LIMIT: usize = 32 * 1024 * 1024;

/// Build the HTTP router.
/// `ui_dir` is the built admin UI (Vite `dist`) directory; pass `None` to disable the UI.
pub fn app_router(
    engine: Arc<HexDBEngine>,
    schema: HexDBSchema,
    ui_dir: Option<PathBuf>,
    shutdown_handle: ShutdownHandle,
) -> Router {
    // Static segments (health, users, _bulk, ...) take priority over the
    // `{tessellation}` and `{id}` parameters, so these names are reserved.
    let mut router = Router::new()
        // Utility, Health, Status
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/status/history", get(status_history))
        .route("/logs", get(logs))
        .route("/plugins", get(plugins))
        .route("/changes", get(crate::changes::changes))
        .route("/changes/stream", get(crate::changes::change_stream))

        // Lattice replication (hex to hex, token-protected)
        .route("/lattice/snapshot", get(crate::lattice::snapshot))
        .route("/lattice/catalog", get(crate::lattice::catalog))
        .route("/lattice/snapshot/{tessellation}", get(crate::lattice::snapshot_page))
        .route("/lattice/changes", get(crate::lattice::changes))
        .route("/transactions", post(transaction).layer(DefaultBodyLimit::max(BULK_BODY_LIMIT)))
        .route("/flush", post(flush))
        .route("/shutdown", post(shutdown))

        // GraphQL (GET serves GraphiQL)
        .route("/graphql", get(graphiql).post(graphql).layer(DefaultBodyLimit::max(BULK_BODY_LIMIT)))

        // Tessellations
        .route("/tessellations", get(list_tessellations).post(create_tessellation))
        .route("/tessellations/{name}", get(get_tessellation).delete(delete_tessellation))
        .route("/tessellations/{name}/indexes", get(list_indexes).post(create_index))
        .route("/tessellations/{name}/indexes/{index}", axum::routing::delete(drop_index))

        // Users and roles
        .route("/users", get(list_users).post(create_user))
        .route("/users/{user}", get(get_user).put(replace_user).patch(patch_user).delete(delete_user))
        .route("/roles", get(list_roles))
        .route("/roles/{name}", get(get_role))

        // Documents
        .route("/{tessellation}", get(list_docs).post(insert_doc))
        .route("/{tessellation}/count", get(count_docs))
        .route(
            "/{tessellation}/_query",
            post(query_docs).layer(DefaultBodyLimit::max(BULK_BODY_LIMIT)),
        )
        .route(
            "/{tessellation}/_aggregate",
            post(aggregate_docs).layer(DefaultBodyLimit::max(BULK_BODY_LIMIT)),
        )
        .route(
            "/{tessellation}/_bulk",
            post(bulk_insert)
                .put(bulk_replace)
                .patch(bulk_patch)
                .layer(DefaultBodyLimit::max(BULK_BODY_LIMIT)),
        )
        .route(
            "/{tessellation}/_update",
            post(update_where).layer(DefaultBodyLimit::max(BULK_BODY_LIMIT)),
        )
        .route("/{tessellation}/{id}", get(get_doc).put(replace_doc).patch(patch_doc).delete(delete_doc));

    // Admin UI routes and static assets. Unknown paths under /ui fall back to
    // index.html so client-side routes work.
    if let Some(dir) = ui_dir {
        let index = dir.join("index.html");
        router = router
            .route("/", get(|| async { Redirect::temporary(UI_ROOT) }))
            .nest_service(UI_PREFIX, ServeDir::new(dir).fallback(ServeFile::new(index)));
    }

    router
        .layer(middleware::from_fn(log_request))
        .layer(Extension(shutdown_handle))
        .layer(Extension(schema))
        .with_state(engine)
}

/// Log every API request once it completes. Writes are logged at INFO so they
/// show up on the Logs page by default; reads (including the UI's own polling)
/// are logged at DEBUG.
async fn log_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    let status = response.status().as_u16();
    let millis = started.elapsed().as_millis() as u64;
    if path.starts_with(UI_PREFIX) {
        return response;
    }
    // Query and aggregate requests are POSTs but only read.
    let read = matches!(method, Method::GET | Method::HEAD | Method::OPTIONS)
        || path.ends_with("/_query")
        || path.ends_with("/_aggregate");
    if status >= 500 {
        tracing::error!(target: "hexdb_api::requests", %method, %path, status, millis, "{} {} -> {}", method, path, status);
    } else if read && status < 400 {
        tracing::debug!(target: "hexdb_api::requests", %method, %path, status, millis, "{} {} -> {}", method, path, status);
    } else {
        tracing::info!(target: "hexdb_api::requests", %method, %path, status, millis, "{} {} -> {}", method, path, status);
    }
    response
}
