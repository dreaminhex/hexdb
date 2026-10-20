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

/// True for the admin UI's paths (`/ui` and below), and not for tessellations
/// whose names merely start with "ui".
pub fn is_ui_path(path: &str) -> bool {
    path == UI_PREFIX || path.starts_with(UI_ROOT)
}

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
        .route("/auth/quota", get(crate::auth::quota))
        .route("/auth/keys/{id}", axum::routing::delete(crate::auth::revoke_key))

        // Utility, Health, Status
        .route("/health", get(health))
        .route("/openapi.json", get(openapi))
        .route("/status", get(status))
        .route("/status/history", get(status_history))
        .route("/logs", get(logs))
        .route("/audit", get(audit))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/config", get(crate::config::get_config).put(crate::config::put_config))
        .route("/config/history", get(crate::config::config_history))
        .route("/config/versions/{version}", get(crate::config::config_version))
        .route("/config/rollback", post(crate::config::rollback_config))
        .route("/ai", get(crate::config::ai_status))
        .route("/ai/test", post(crate::config::ai_test))
        .route("/ai/complete", post(crate::config::ai_complete))
        .route("/ai/estimate", get(crate::config::ai_estimate))
        .route("/ai/chat", get(crate::agent::chat_status).post(crate::agent::chat))
        .route("/shares", get(crate::shares::list).post(crate::shares::create))
        .route("/shares/{code}", axum::routing::delete(crate::shares::delete))
        .route("/_s/{code}", get(crate::shares::info))
        .route("/_s/{code}/unlock", post(crate::shares::unlock))
        .route("/_s/{code}/content", get(crate::shares::content))
        .route("/_s/{code}/thumbnail", get(crate::shares::thumbnail))
        .route("/_s/{code}/files", get(crate::shares::files))
        .route("/mcp", post(crate::mcp::post).get(crate::mcp::not_offered).delete(crate::mcp::not_offered))
        .route("/join", post(join_info))

        // Streams (publish/subscribe)
        .route("/queues", get(crate::queues::list).post(crate::queues::create))
        .route("/queues/{name}", get(crate::queues::get).put(crate::queues::update).delete(crate::queues::delete))
        .route("/queues/{name}/messages", post(crate::queues::send).layer(DefaultBodyLimit::max(body_limit)))
        .route("/queues/{name}/receive", post(crate::queues::receive))
        .route("/queues/{name}/ack", post(crate::queues::ack))
        .route("/queues/{name}/nack", post(crate::queues::nack))
        .route("/queues/{name}/extend", post(crate::queues::extend))
        .route("/streams", get(crate::streams::list).post(crate::streams::create))
        .route("/streams/{name}", get(crate::streams::get).put(crate::streams::update).delete(crate::streams::delete))
        .route(
            "/streams/{name}/messages",
            get(crate::streams::read).post(crate::streams::publish).layer(DefaultBodyLimit::max(body_limit)),
        )
        .route("/streams/{name}/groups/{group}/commit", post(crate::streams::commit))
        .route("/streams/{name}/subscribe", get(crate::streams::subscribe))
        .route("/streams/{name}/ws", get(crate::streams::subscribe_socket))

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
        .route("/changes/ws", get(crate::changes::change_socket))

        // Lattice replication (hex to hex, token-protected)
        .route("/lattice/snapshot", get(crate::lattice::snapshot))
        .route("/lattice/catalog", get(crate::lattice::catalog))
        .route("/lattice/snapshot/{tessellation}", get(crate::lattice::snapshot_page))
        .route("/lattice/slice-snapshot/{tessellation}", get(crate::lattice::slice_snapshot))
        .route("/lattice/changes", get(crate::lattice::changes))
        .route("/lattice/chunks/{id}", get(crate::lattice::chunk))
        .route("/lattice/revoke", post(crate::lattice::revoke))
        .route("/lattice/audit", post(crate::lattice::audit))
        .route("/lattice/ai-usage", post(crate::lattice::ai_usage))
        .route("/lattice/share-download", post(crate::lattice::share_download))
        .route("/lattice/throttle", post(crate::lattice::throttle))
        .route("/transactions", post(transaction).layer(DefaultBodyLimit::max(body_limit)))
        .route("/flush", post(flush))
        .route("/backup", post(backup))
        .route("/backups", get(list_backups))
        .route("/compact", post(compact))
        .route("/scrub", post(scrub))
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
        // Files tessellations (uploads stream, so they have no body limit here;
        // files.max_file_mb and storage.disk_mb apply)
        .route(
            "/{tessellation}/_files/{*path}",
            get(crate::files::download_path).put(crate::files::upload).layer(DefaultBodyLimit::disable()),
        )
        .route("/{tessellation}/_trash", get(crate::files::trash).delete(crate::files::empty_trash))
        .route("/{tessellation}/_reextract", post(crate::files::reextract))
        .route("/{tessellation}/_ai_tag", post(crate::config::ai_tag))
        .route("/{tessellation}/_ai_thumbnails", post(crate::config::ai_thumbnails))
        .route("/{tessellation}/_uploads", post(crate::files::begin_upload))
        .route(
            "/{tessellation}/_uploads/{id}",
            get(crate::files::upload_status)
                .patch(crate::files::upload_part)
                .delete(crate::files::abort_upload)
                .layer(DefaultBodyLimit::disable()),
        )
        .route("/{tessellation}/_uploads/{id}/complete", post(crate::files::complete_upload))
        .route("/{tessellation}/_trash/{id}", axum::routing::delete(crate::files::purge))
        .route("/{tessellation}/_trash/{id}/restore", post(crate::files::restore))
        .route("/{tessellation}/{id}/content", get(crate::files::download))
        .route("/{tessellation}/{id}/versions", get(crate::files::versions))
        .route("/{tessellation}/{id}/thumbnail", get(crate::files::thumbnail))
        .route("/{tessellation}/{id}/versions/{version}/restore", post(crate::files::restore_version))
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
    let rest = router
        .layer(middleware::from_fn_with_state(engine.clone(), crate::lattice::sign_responses))
        .layer(middleware::from_fn_with_state(engine.clone(), crate::forward::forward_writes))
        .layer(middleware::from_fn_with_state(engine.clone(), crate::auth::authenticate))
        .layer(middleware::from_fn_with_state(timeout, crate::server::request_timeout))
        .layer(middleware::from_fn_with_state(engine.clone(), log_request))
        .layer(middleware::from_fn_with_state((proxies, engine.clone()), crate::client::resolve_client))
        .layer(middleware::from_fn_with_state(engine.clone(), crate::auth::security_headers))
        .layer(Extension(shutdown_handle))
        .layer(Extension(schema))
        .with_state(engine);
    // gRPC calls (content-type application/grpc) run as REST requests through all of the above.
    crate::grpc::with_grpc(rest, body_limit)
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
    if is_ui_path(&path) {
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
        // A signed-in user refused something is worth keeping; anonymous 401s
        // are only noise. Capped per user, so denials can't fill the disk.
        if status == 403 {
            let details = match denial_budget(&user) {
                Denial::Record => Some(serde_json::json!({ "method": method.as_str() })),
                Denial::RecordLast => Some(serde_json::json!({
                    "method": method.as_str(),
                    "note": format!("More than {} denials this minute; the rest of this minute's aren't recorded.", DENIALS_PER_MINUTE),
                })),
                Denial::Skip => None,
            };
            if let Some(details) = details {
                let event = hexdb_core::AuditEvent::new(&user, "access.denied", &path).outcome("denied").client(&client).details(details);
                engine.record_audit(event).await;
            }
        }
    } else if read && status < 400 {
        tracing::debug!(target: "hexdb_api::requests", %method, %path, status, millis, %user, %client, "{} {} -> {}", method, path, status);
    } else {
        tracing::info!(target: "hexdb_api::requests", %method, %path, status, millis, %user, %client, "{} {} -> {}", method, path, status);
    }
    response
}

/// Denied requests recorded in the audit trail per user per minute.
const DENIALS_PER_MINUTE: u32 = 30;

#[derive(Debug, PartialEq)]
enum Denial {
    Record,
    /// The last one recorded this minute (it says so).
    RecordLast,
    Skip,
}

fn denial_budget(user: &str) -> Denial {
    static COUNTS: std::sync::Mutex<Option<std::collections::HashMap<String, (u64, u32)>>> = std::sync::Mutex::new(None);
    let minute = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() / 60).unwrap_or(0);
    let mut guard = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let counts = guard.get_or_insert_with(Default::default);
    if counts.len() > 10_000 {
        counts.retain(|_, (m, _)| *m == minute);
    }
    let entry = counts.entry(user.to_string()).or_insert((minute, 0));
    if entry.0 != minute {
        *entry = (minute, 0);
    }
    entry.1 = entry.1.saturating_add(1);
    match entry.1 {
        n if n < DENIALS_PER_MINUTE => Denial::Record,
        n if n == DENIALS_PER_MINUTE => Denial::RecordLast,
        _ => Denial::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denials_are_capped_per_user() {
        let user = "denial-test-user";
        let seen: Vec<Denial> = (0..DENIALS_PER_MINUTE + 5).map(|_| denial_budget(user)).collect();
        // A minute boundary mid-test restarts the count; only check when it didn't.
        if seen.iter().filter(|d| **d == Denial::RecordLast).count() == 1 {
            assert_eq!(seen.iter().filter(|d| **d == Denial::Record).count() as u32, DENIALS_PER_MINUTE - 1);
            assert_eq!(seen.iter().filter(|d| **d == Denial::Skip).count(), 5);
        }
        assert_eq!(denial_budget("someone-else"), Denial::Record);
    }

    #[test]
    fn ui_paths_are_exact() {
        assert!(is_ui_path("/ui") && is_ui_path("/ui/") && is_ui_path("/ui/assets/x.js"));
        assert!(!is_ui_path("/uikit") && !is_ui_path("/uploads/1") && !is_ui_path("/"));
    }
}
