// HexDB API: the IP blocklist
//
// Requests from a blocked address or range are refused with 403 `blocked`
// right after the client address is resolved (see `crate::client`), before
// authentication, so they cost almost nothing and learn nothing. The entries
// live in the `_blocklist` system tessellation (see hexdb_core::blocklist),
// replicated to every hex; each hex keeps them in memory, reloaded when the
// tessellation changes and every 30 seconds (for expiries). `/health`,
// `/ready` and the hex-to-hex `/lattice/*` routes are never blocked, so a
// mistaken entry can't take a hex out of its load balancer or its lattice.

use crate::auth::Auth;
use crate::handlers::{no_content, ApiError, ApiResult, Engine};
use axum::{
    extract::{Path, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Extension, Json,
};
use hexdb_core::blocklist::{Cidr, NewBlock, BLOCKLIST_TESSELLATION};
use hexdb_core::engine::HexDBEngine;
use serde_json::json;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::{debug, warn};

/// How often expired entries are dropped from memory.
const RELOAD_EVERY: Duration = Duration::from_secs(30);

/// Ranges and when each stops applying (epoch ms; none: never).
type Entries = Arc<Vec<(Cidr, Option<i64>)>>;

/// The entries in effect on this hex.
#[derive(Default)]
pub struct Blocklist {
    entries: RwLock<Entries>,
}

impl Blocklist {
    /// Load the entries and keep them current.
    pub fn start(engine: Arc<HexDBEngine>) -> Arc<Blocklist> {
        let list = Arc::new(Blocklist::default());
        let (task_list, task_engine) = (list.clone(), engine.clone());
        tokio::spawn(async move {
            let mut changes = task_engine.changes.subscribe();
            let mut tick = tokio::time::interval(RELOAD_EVERY);
            loop {
                task_list.reload(&task_engine).await;
                tokio::select! {
                    _ = tick.tick() => {}
                    change = changes.recv() => {
                        // Lagging or a change elsewhere: reload only for the blocklist (lagged: to be safe).
                        if let Ok(change) = change {
                            if change.tessellation != BLOCKLIST_TESSELLATION {
                                continue;
                            }
                        }
                    }
                }
            }
        });
        list
    }

    async fn reload(&self, engine: &HexDBEngine) {
        match engine.blocklist().await {
            Ok(entries) => {
                let compiled: Vec<(Cidr, Option<i64>)> = entries.iter().filter_map(|e| Cidr::parse(&e.address).map(|c| (c, e.expires_at))).collect();
                *self.entries.write().unwrap() = Arc::new(compiled);
            }
            Err(e) => warn!("⚠️ Couldn't read the IP blocklist: {:#}", e),
        }
    }

    /// True if requests from `ip` are refused.
    pub fn blocks(&self, ip: IpAddr) -> bool {
        let now = chrono::Utc::now().timestamp_millis();
        self.entries.read().unwrap().iter().any(|(cidr, expires)| expires.is_none_or(|at| at > now) && cidr.contains(ip))
    }
}

fn exempt(path: &str) -> bool {
    path == "/health" || path == "/ready" || path.starts_with("/lattice/")
}

/// Refuse requests from blocked addresses (see the module comment).
pub async fn check(State(list): State<Arc<Blocklist>>, request: Request, next: Next) -> Response {
    if !exempt(request.uri().path()) {
        let client = crate::auth::client_address(request.extensions());
        if client.parse::<IpAddr>().is_ok_and(|ip| list.blocks(ip)) {
            debug!("🚫 Refused a request from blocked address {} ({}).", client, request.uri().path());
            return ApiError::new(StatusCode::FORBIDDEN, "blocked", "Requests from your address are blocked.").into_response();
        }
    }
    next.run(request).await
}

/// `GET /blocklist`: the entries in effect (administrators).
pub async fn list(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    Ok(Json(json!({ "entries": engine.blocklist().await? })).into_response())
}

/// `POST /blocklist`: `{"address", "note", "expires_in_hours"}` (administrators; audited).
pub async fn add(
    State(engine): Engine,
    Auth(principal): Auth,
    Extension(list): Extension<Arc<Blocklist>>,
    request: axum::http::Request<axum::body::Body>,
) -> ApiResult {
    principal.require_admin()?;
    let own = crate::auth::client_address(request.extensions()).parse::<IpAddr>().ok();
    let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024).await.map_err(|e| ApiError::invalid(e.to_string()))?;
    let input: NewBlock = serde_json::from_slice(&bytes).map_err(|e| ApiError::invalid(e.to_string()))?;
    let entry = engine.add_block(&principal.login, input, own).await?;
    list.reload(&engine).await;
    engine.audit(&principal.login, "security.block", &entry.address, json!({ "id": entry.id, "note": entry.note, "expires_at": entry.expires_at })).await;
    Ok((StatusCode::CREATED, Json(json!(entry))).into_response())
}

/// `DELETE /blocklist/{id}` (administrators; audited).
pub async fn remove(Path(id): Path<String>, State(engine): Engine, Auth(principal): Auth, Extension(list): Extension<Arc<Blocklist>>) -> ApiResult {
    principal.require_admin()?;
    let entry = engine.remove_block(&id).await?.ok_or_else(|| ApiError::not_found(format!("No blocklist entry {}.", id)))?;
    list.reload(&engine).await;
    engine.audit(&principal.login, "security.unblock", &entry.address, json!({ "id": id })).await;
    Ok(no_content(false))
}
