use axum::{
    Router,
    routing::{get, post, put, patch, delete},
};
use std::sync::Arc;
use hexdb_core::engine::HexDBEngine;
use tower_http::services::{ServeFile};
use crate::handlers::*;

pub fn app_router(engine: Arc<HexDBEngine>) -> Router {

    let dist_dir = "../hexdb_admin/dist";
    //let serve_dir = ServeDir::new(dist_dir);
    let index_html = ServeFile::new(format!("{}/index.html", dist_dir));

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
        //.route("/tessellation", get(get_tessellations))

        // TODO: Security routes
        //.route("/user", get(get_user))
        //.route("/user", post(insert_user))
        //.route("/user", delete(delete_user))
        //.route("/user", patch(patch_user))
        //.route("/user", put(update_user))
        //.route("/roles", get(get_roles))

        // Utility, Health, Status
        .route("/{tessellation}/count", get(count_docs))
        .route("/flush", get(flush))
        .route("/status", get(status))

        // Admin UI routes and static assets
        .route_service("/hexdb.svg", ServeFile::new(format!("{}/hexdb.svg", dist_dir)))
        .fallback_service(index_html)

        .with_state(engine)
}
