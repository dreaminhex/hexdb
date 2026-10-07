use axum::{
    extract::DefaultBodyLimit,
    response::Redirect,
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
        .route("/flush", post(flush))
        .route("/shutdown", post(shutdown))

        // GraphQL (GET serves GraphiQL)
        .route("/graphql", get(graphiql).post(graphql).layer(DefaultBodyLimit::max(BULK_BODY_LIMIT)))

        // Tessellations
        .route("/tessellations", get(list_tessellations).post(create_tessellation))
        .route("/tessellations/{name}", get(get_tessellation).delete(delete_tessellation))

        // Users and roles
        .route("/users", get(list_users).post(create_user))
        .route("/users/{user}", get(get_user).put(replace_user).patch(patch_user).delete(delete_user))
        .route("/roles", get(list_roles))
        .route("/roles/{name}", get(get_role))

        // Documents
        .route("/{tessellation}", get(list_docs).post(insert_doc))
        .route("/{tessellation}/count", get(count_docs))
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
        .layer(Extension(shutdown_handle))
        .layer(Extension(schema))
        .with_state(engine)
}
