use axum::{
    response::Redirect,
    routing::{get, post},
    Extension, Router,
};
use std::{path::PathBuf, sync::Arc};
use hexdb_core::engine::HexDBEngine;
use tower_http::services::{ServeDir, ServeFile};
use crate::handlers::*;

/// URL prefix the admin UI is served under. Must match `base` in hexdb_admin/vite.config.ts.
pub const UI_PREFIX: &str = "/ui";
const UI_ROOT: &str = "/ui/";

/// Build the HTTP router.
/// `ui_dir` is the built admin UI (Vite `dist`) directory; pass `None` to disable the UI.
pub fn app_router(engine: Arc<HexDBEngine>, ui_dir: Option<PathBuf>, shutdown_handle: ShutdownHandle) -> Router {
    // Static segments (health, status, ui, ...) take priority over the
    // `{tessellation}` parameter, so these names are reserved.
    let mut router = Router::new()
        // Utility, Health, Status
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/flush", get(flush))
        .route("/shutdown", post(shutdown))

        // Tessellation routes
        .route("/tessellation/{name}", post(create_tessellation).delete(delete_tessellation))
        //.route("/tessellation", get(get_tessellations))

        // TODO: Security routes
        //.route("/user", get(get_user))
        //.route("/user", post(insert_user))
        //.route("/user", delete(delete_user))
        //.route("/user", patch(patch_user))
        //.route("/user", put(update_user))
        //.route("/roles", get(get_roles))

        // Document routes
        .route("/{tessellation}", post(insert_doc).put(update_doc).patch(patch_doc))
        .route("/{tessellation}/count", get(count_docs))
        .route("/{tessellation}/{id}", get(get_doc).delete(delete_doc));

    // Admin UI routes and static assets. Unknown paths under /ui fall back to
    // index.html so client-side routes work.
    if let Some(dir) = ui_dir {
        let index = dir.join("index.html");
        router = router
            .route("/", get(|| async { Redirect::temporary(UI_ROOT) }))
            .nest_service(UI_PREFIX, ServeDir::new(dir).fallback(ServeFile::new(index)));
    }

    router.layer(Extension(shutdown_handle)).with_state(engine)
}
