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

/// Request body limit for bulk endpoints (`limits.max_request_mb`). Single-document writes allow at least `limits.max_document_kb`; other endpoints use axum's 2 MB default.
pub fn bulk_body_limit(engine: &HexDBEngine) -> usize {
    (engine.config.limits.max_request_mb.max(1) as usize).saturating_mul(1024 * 1024)
}

/// Build the HTTP router.
/// `ui_dir` is the built admin UI (Vite `dist`) directory; pass `None` to disable the UI.
pub fn app_router(
    engine: Arc<HexDBEngine>,
    schema: HexDBSchema,
    ui_dir: Option<PathBuf>,
    shutdown_handle: ShutdownHandle,
) -> Router {
    let body_limit = bulk_body_limit(&engine);
    // Single-document writes may carry a document up to limits.max_document_kb
    // (checked by the engine); allow at least that much body, with room for JSON.
    let document_limit = body_limit.max((engine.config.limits.max_document_kb as usize + 64).saturating_mul(1024));
    // Static segments (health, users, _bulk, ...) take priority over the
    // `{tessellation}` and `{id}` parameters, so these names are reserved.
    let mut router = Router::new()
        // Authentication
        .route("/auth/login", post(crate::auth::login))
        .route("/auth/logout", post(crate::auth::logout))
        .route("/auth/me", get(crate::auth::me))
        .route("/auth/password", post(crate::auth::change_password))
        .route("/auth/mfa", get(crate::auth::mfa_status))
        .route("/auth/mfa/setup", post(crate::auth::mfa_setup))
        .route("/auth/mfa/enable", post(crate::auth::mfa_enable))
        .route("/auth/mfa/disable", post(crate::auth::mfa_disable))
        .route("/auth/mfa/backup-codes", post(crate::auth::mfa_backup_codes))
        .route("/auth/keys", get(crate::auth::list_keys).post(crate::auth::create_key))
        .route("/auth/keys/{id}", axum::routing::delete(crate::auth::revoke_key))

        // Utility, Health, Status
        .route("/health", get(health))
        .route("/openapi.json", get(openapi))
        .route("/status", get(status))
        .route("/status/history", get(status_history))
        .route("/logs", get(logs))
        .route("/audit", get(audit))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/join", post(join_info))

        // Streams (publish/subscribe)
        .route("/streams", get(crate::streams::list).post(crate::streams::create))
        .route("/streams/{name}", get(crate::streams::get).put(crate::streams::update).delete(crate::streams::delete))
        .route(
            "/streams/{name}/messages",
            get(crate::streams::read).post(crate::streams::publish).layer(DefaultBodyLimit::max(body_limit)),
        )
        .route("/streams/{name}/groups/{group}/commit", post(crate::streams::commit))
        .route("/streams/{name}/subscribe", get(crate::streams::subscribe))

        // Functions and schedules
        .route("/functions", get(crate::functions::list).post(crate::functions::create))
        .route("/functions/{name}", get(crate::functions::get).put(crate::functions::update).delete(crate::functions::delete))
        .route("/functions/{name}/run", post(crate::functions::run).layer(DefaultBodyLimit::max(body_limit)))
        .route("/schedules", get(crate::functions::list_schedules).post(crate::functions::create_schedule))
        .route(
            "/schedules/{name}",
            get(crate::functions::get_schedule).put(crate::functions::update_schedule).delete(crate::functions::delete_schedule),
        )
        .route("/schedules/{name}/run", post(crate::functions::run_schedule))

        // Triggers
        .route("/triggers", get(crate::triggers::list).post(crate::triggers::create))
        .route("/triggers/{name}", get(crate::triggers::get).put(crate::triggers::update).delete(crate::triggers::delete))
        .route("/plugins", get(plugins))
        .route("/changes", get(crate::changes::changes))
        .route("/changes/stream", get(crate::changes::change_stream))

        // Lattice replication (hex to hex, token-protected)
        .route("/lattice/snapshot", get(crate::lattice::snapshot))
        .route("/lattice/catalog", get(crate::lattice::catalog))
        .route("/lattice/snapshot/{tessellation}", get(crate::lattice::snapshot_page))
        .route("/lattice/changes", get(crate::lattice::changes))
        .route("/lattice/revoke", post(crate::lattice::revoke))
        .route("/lattice/audit", post(crate::lattice::audit))
        .route("/lattice/throttle", post(crate::lattice::throttle))
        .route("/transactions", post(transaction).layer(DefaultBodyLimit::max(body_limit)))
        .route("/flush", post(flush))
        .route("/backup", post(backup))
        .route("/backups", get(list_backups))
        .route("/compact", post(compact))
        .route("/shutdown", post(shutdown))

        // GraphQL (the admin UI's Queries page is the console)
        .route("/graphql", post(graphql).layer(DefaultBodyLimit::max(body_limit)))
        .route("/sql", post(crate::sql::run).layer(DefaultBodyLimit::max(body_limit)))
        .route("/sql/tables", get(crate::sql::tables))
        .route("/sql/columns", get(crate::sql::columns))

        // Tessellations
        .route("/tessellations", get(list_tessellations).post(create_tessellation))
        .route("/tessellations/{name}", get(get_tessellation).delete(delete_tessellation))
        .route("/tessellations/{name}/indexes", get(list_indexes).post(create_index))
        .route("/tessellations/{name}/indexes/{index}", axum::routing::delete(drop_index))
        .route("/tessellations/{name}/advice", get(advice))
        .route("/tessellations/{name}/schemas", get(get_schemas).post(add_schema).delete(drop_schemas))
        .route("/tessellations/{name}/schemas/check", post(check_schema))
        .route("/tessellations/{name}/schemas/rollback", post(rollback_schema))
        .route("/analyzers", get(list_analyzers))
        .route("/analyzers/_analyze", post(analyze))

        // Users and roles
        .route("/users", get(list_users).post(create_user))
        .route("/users/{user}", get(get_user).put(replace_user).patch(patch_user).delete(delete_user))
        .route("/roles", get(list_roles).post(create_role))
        .route("/roles/{name}", get(get_role).put(update_role).patch(update_role).delete(delete_role))

        // Documents
        .route("/{tessellation}", get(list_docs).post(insert_doc).layer(DefaultBodyLimit::max(document_limit)))
        .route("/{tessellation}/count", get(count_docs))
        .route(
            "/{tessellation}/_query",
            post(query_docs).layer(DefaultBodyLimit::max(body_limit)),
        )
        .route(
            "/{tessellation}/_aggregate",
            post(aggregate_docs).layer(DefaultBodyLimit::max(body_limit)),
        )
        .route(
            "/{tessellation}/_bulk",
            post(bulk_insert)
                .put(bulk_replace)
                .patch(bulk_patch)
                .layer(DefaultBodyLimit::max(body_limit)),
        )
        .route(
            "/{tessellation}/_upsert",
            post(upsert_docs).layer(DefaultBodyLimit::max(body_limit)),
        )
        .route(
            "/{tessellation}/_update",
            post(update_where).layer(DefaultBodyLimit::max(body_limit)),
        )
        .route(
            "/{tessellation}/{id}",
            get(get_doc).put(replace_doc).patch(patch_doc).delete(delete_doc).layer(DefaultBodyLimit::max(document_limit)),
        );

    // Admin UI routes and static assets. Unknown paths under /ui fall back to
    // index.html so client-side routes work.
    if let Some(dir) = ui_dir {
        let index = dir.join("index.html");
        router = router
            .route("/", get(|| async { Redirect::temporary(UI_ROOT) }))
            .nest_service(UI_PREFIX, ServeDir::new(dir).fallback(ServeFile::new(index)));
    }

    // Layers run outside-in from the last one added: security headers, then
    // the client address, then request logging, then the request timeout, then
    // authentication, then forwarding writes to the Overseer (on replicas), then the route.
    // Validated at startup (see main).
    let proxies = Arc::new(crate::client::TrustedProxies::parse(&engine.config.network.trusted_proxies).unwrap_or_default());
    let timeout = std::time::Duration::from_secs(engine.config.limits.request_timeout_seconds.max(1));
    router
        .layer(middleware::from_fn_with_state(engine.clone(), crate::forward::forward_writes))
        .layer(middleware::from_fn_with_state(engine.clone(), crate::auth::authenticate))
        .layer(middleware::from_fn_with_state(timeout, crate::server::request_timeout))
        .layer(middleware::from_fn_with_state(engine.clone(), log_request))
        .layer(middleware::from_fn_with_state((proxies, engine.clone()), crate::client::resolve_client))
        .layer(middleware::from_fn_with_state(engine.clone(), crate::auth::security_headers))
        .layer(Extension(shutdown_handle))
        .layer(Extension(schema))
        .with_state(engine)
}

/// Who made a request, recorded by the authentication middleware for the request log.
#[derive(Clone)]
pub struct AuditUser(pub String);

/// Log every API request once it completes. Writes are logged at INFO so they
/// show up on the Logs page by default; reads (including the UI's own polling)
/// are logged at DEBUG.
async fn log_request(axum::extract::State(engine): axum::extract::State<Arc<HexDBEngine>>, request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let client = crate::auth::client_address(request.extensions());
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    let user = response.extensions().get::<AuditUser>().map(|u| u.0.clone()).unwrap_or_else(|| "-".into());
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
        tracing::error!(target: "hexdb_api::requests", %method, %path, status, millis, %user, %client, "{} {} -> {}", method, path, status);
    } else if status == 401 || status == 403 {
        tracing::warn!(target: "hexdb_api::requests", %method, %path, status, millis, %user, %client, "{} {} -> {}", method, path, status);
        // A signed-in user refused something is worth keeping; anonymous 401s are only noise.
        if status == 403 {
            let event = hexdb_core::AuditEvent::new(&user, "access.denied", &path)
                .outcome("denied")
                .client(&client)
                .details(serde_json::json!({ "method": method.as_str() }));
            engine.record_audit(event).await;
        }
    } else if read && status < 400 {
        tracing::debug!(target: "hexdb_api::requests", %method, %path, status, millis, %user, %client, "{} {} -> {}", method, path, status);
    } else {
        tracing::info!(target: "hexdb_api::requests", %method, %path, status, millis, %user, %client, "{} {} -> {}", method, path, status);
    }
    response
}
