// HexDB API: lattice replication endpoints (hex-to-hex)
//
//   GET /lattice/snapshot                 where a full sync starts (sequence, catalog)
//   GET /lattice/catalog                  tessellations and index definitions
//   GET /lattice/snapshot/{tessellation}  documents with versions, paged by ID
//   GET /lattice/changes?after=&wait=     the change feed with typed documents
//   POST /lattice/revoke                  record a sign-out made on a replica
//   GET /lattice/chunks/{id}              a file chunk, sealed with the lattice transfer key
//                                         (any hex serves it, so hexes repair each other)
//
// Every request needs a valid `X-HexDB-Lattice-Signature` (fresh, single use,
// made with the lattice key; see `hexdb_core::network::lattice_auth`), and
// only the Overseer serves them.

use crate::handlers::{ApiError, ApiResult, Engine};
use axum::{
    body::Bytes,
    extract::{rejection::QueryRejection, Path, Query},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::IntoResponse,
    Json,
};
use hexdb_core::{
    engine::HexDBEngine,
    replication::{ChangeBatch, FeedScope, SnapshotPage},
    LATTICE_SIGNATURE_HEADER,
};
use serde::Deserialize;
use std::time::Duration;
use ulid::Ulid;

/// The largest lattice answer signed (snapshot pages, change batches and chunks are far smaller).
const MAX_SIGNED_RESPONSE: usize = 256 * 1024 * 1024;

/// Sign `/lattice/*` answers to signed requests, so a replica can tell they
/// came from a hex holding the lattice key (see `LATTICE_RESPONSE_HEADER`).
pub async fn sign_responses(
    axum::extract::State(engine): axum::extract::State<std::sync::Arc<HexDBEngine>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let lattice = request.uri().path().starts_with("/lattice/");
    let signature = if lattice {
        request.headers().get(LATTICE_SIGNATURE_HEADER).and_then(|v| v.to_str().ok()).map(String::from)
    } else {
        // A request a replica forwarded: its JSON answer is signed for it too.
        request.extensions().get::<crate::forward::ForwardSignature>().map(|s| s.0.clone())
    };
    let response = next.run(request).await;
    let Some(signature) = signature else { return response };
    if !lattice && !crate::forward::signed_answer(response.headers()) {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_SIGNED_RESPONSE).await else {
        return ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "too_large", "The lattice response is too large to sign.").into_response();
    };
    let tag = engine.lattice_keys.sign_response(&signature, parts.status.as_u16(), &bytes);
    if let Ok(value) = axum::http::HeaderValue::from_str(&tag) {
        parts.headers.insert(hexdb_core::replication::LATTICE_RESPONSE_HEADER, value);
    }
    axum::response::Response::from_parts(parts, axum::body::Body::from(bytes))
}

/// A fresh lattice signature, from any hex of this lattice.
fn verify_signature(engine: &HexDBEngine, headers: &HeaderMap, method: &Method, uri: &Uri, body: &[u8]) -> Result<(), ApiError> {
    let presented = headers.get(LATTICE_SIGNATURE_HEADER).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let target = uri.path_and_query().map(|p| p.as_str()).unwrap_or(uri.path());
    if !engine.lattice_keys.verify_request(presented, method.as_str(), target, body, &engine.lattice_nonces) {
        tracing::warn!("🚫 Rejected a lattice request to {} without a valid signature.", uri.path());
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "A valid lattice signature is required."));
    }
    Ok(())
}

/// Checked again before answering a poll for changes. A hex that stopped being the
/// Overseer while a long poll waited (a handover) goes on to write its own bookkeeping
/// as a replica; those positions must not reach the replicas as part of the history,
/// or a replica ends up ahead of where the new Overseer took over and has to copy
/// everything again.
fn still_overseer(engine: &HexDBEngine) -> Result<(), ApiError> {
    if engine.is_writable() {
        return Ok(());
    }
    Err(ApiError::new(StatusCode::CONFLICT, "not_overseer", format!("This hex is {} now; replicate from the Overseer.", hexdb_core::network::discovery::a_role(&engine.role()))))
}

/// A signed request to an endpoint only the Overseer serves.
pub(crate) fn authorize(engine: &HexDBEngine, headers: &HeaderMap, method: &Method, uri: &Uri, body: &[u8]) -> Result<(), ApiError> {
    verify_signature(engine, headers, method, uri, body)?;
    if !engine.is_writable() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "not_overseer",
            format!("This hex is {}; replicate from the Overseer.", hexdb_core::network::discovery::a_role(&engine.role())),
        ));
    }
    Ok(())
}

/// A file chunk this hex holds, sealed for the asking hex. Any hex answers,
/// not only the Overseer; 404 when it doesn't have the chunk (or can't rebuild it).
pub async fn chunk(Path(id): Path<String>, method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine) -> ApiResult {
    let presented = headers.get(LATTICE_SIGNATURE_HEADER).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let target = uri.path_and_query().map(|p| p.as_str()).unwrap_or(uri.path());
    if !engine.lattice_keys.verify_request(presented, method.as_str(), target, b"", &engine.lattice_nonces) {
        tracing::warn!("🚫 Rejected a lattice request to {} without a valid signature.", uri.path());
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "A valid lattice signature is required."));
    }
    let missing = || ApiError::new(StatusCode::NOT_FOUND, "not_found", "This hex doesn't have that chunk.");
    let id = hexdb_core::blobs::parse_chunk_id(&id).ok_or_else(missing)?;
    if !engine.blobs().contains(&id) {
        return Err(missing());
    }
    // Copying file content to another hex is catch-up work: low priority (see hexdb_core::tasks).
    let reading = engine.clone();
    let (sealed, compressed) = hexdb_core::tasks::in_background(async move {
        let data = reading.read_local_chunk(id).await?;
        let compressed = reading.blobs().is_compressed(&id).unwrap_or(false);
        anyhow::Ok((reading.lattice_keys.seal(&data, &id), compressed))
    })
    .await
    .map_err(|_| missing())?;
    let mut response = sealed.into_response();
    response.headers_mut().insert("x-hexdb-compress", axum::http::HeaderValue::from_static(if compressed { "1" } else { "0" }));
    Ok(response)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeParams {
    /// group (default), system or catalog: see hexdb_core::replication::FeedScope.
    pub scope: Option<String>,
}

/// The feed scope asked for. The system and catalog scopes are partition 0's to serve.
fn feed_scope(engine: &HexDBEngine, scope: Option<&str>) -> Result<FeedScope, ApiError> {
    let scope = FeedScope::parse(scope).ok_or_else(|| ApiError::invalid("scope must be group, system or catalog."))?;
    if scope != FeedScope::Group && engine.my_partition() != Some(0) {
        return Err(ApiError::new(StatusCode::CONFLICT, "not_partition_zero", "Only the Overseer of partition 0 serves the system stream."));
    }
    Ok(scope)
}

/// Where a full sync starts.
pub async fn snapshot(params: Result<Query<ScopeParams>, QueryRejection>, method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine) -> ApiResult {
    authorize(&engine, &headers, &method, &uri, b"")?;
    not_rebuilding(&engine)?;
    let Query(params) = params?;
    let scope = feed_scope(&engine, params.scope.as_deref())?;
    Ok(Json(engine.snapshot_meta_for(scope)).into_response())
}

/// Tessellations and index definitions (replicas poll this for new ones).
pub async fn catalog(params: Result<Query<ScopeParams>, QueryRejection>, method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine) -> ApiResult {
    authorize(&engine, &headers, &method, &uri, b"")?;
    not_rebuilding(&engine)?;
    let Query(params) = params?;
    let scope = feed_scope(&engine, params.scope.as_deref())?;
    Ok(Json(engine.snapshot_meta_for(scope)).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageParams {
    pub after: Option<String>,
    pub limit: Option<usize>,
    /// Only documents in these slices (a mask; see hexdb_core::slicing).
    pub slices: Option<u64>,
}

/// While a new Overseer collects slices, it has no complete snapshot or feed to offer.
fn not_rebuilding(engine: &hexdb_core::engine::HexDBEngine) -> Result<(), ApiError> {
    match engine.rebuilding() {
        Some(note) => Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "rebuilding", note).with_retry_after(5)),
        None => Ok(()),
    }
}

/// One page of a tessellation's documents with their versions.
pub async fn snapshot_page(
    Path(tess): Path<String>,
    params: Result<Query<PageParams>, QueryRejection>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    axum::extract::State(engine): Engine,
) -> ApiResult {
    authorize(&engine, &headers, &method, &uri, b"")?;
    not_rebuilding(&engine)?;
    let Query(params) = params?;
    if !engine.tessellation_exists(&tess) {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found", format!("Tessellation '{}' not found.", tess)));
    }
    let after = match params.after.as_deref() {
        Some(a) => Some(Ulid::from_string(a).map_err(|_| ApiError::invalid("after must be a document ID."))?),
        None => None,
    };
    let slices = params.slices.unwrap_or(hexdb_core::slicing::ALL_SLICES);
    // A full sync is catch-up work: low priority, so clients and /health go first.
    let (reading, limit) = (engine.clone(), params.limit.unwrap_or(500).clamp(1, 5000));
    let (documents, next) = hexdb_core::tasks::in_background(async move { reading.snapshot_page(&tess, after, limit, slices).await }).await?;
    Ok(Json(SnapshotPage { documents, next }).into_response())
}

/// A replica's documents in some slices, as of its position: what a newly
/// elected Overseer collects after a failover (see hexdb_core::replication).
pub async fn slice_snapshot(
    Path(tess): Path<String>,
    params: Result<Query<PageParams>, QueryRejection>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    axum::extract::State(engine): Engine,
) -> ApiResult {
    // Served by replicas: no Overseer check.
    verify_signature(&engine, &headers, &method, &uri, b"")?;
    let Query(params) = params?;
    let (source_id, applied_seq) = {
        let status = engine.replication.lock().unwrap();
        (status.history.clone().unwrap_or_default(), status.applied_seq)
    };
    if engine.is_writable() || source_id.is_empty() {
        return Err(ApiError::new(StatusCode::CONFLICT, "not_a_replica", "Only a replica serves its slices this way."));
    }
    let after = match params.after.as_deref() {
        Some(a) => Some(Ulid::from_string(a).map_err(|_| ApiError::invalid("after must be a document ID."))?),
        None => None,
    };
    let slices = params.slices.unwrap_or(hexdb_core::slicing::ALL_SLICES);
    let (documents, next) = if engine.tessellation_exists(&tess) {
        let (reading, limit) = (engine.clone(), params.limit.unwrap_or(500).clamp(1, 5000));
        hexdb_core::tasks::in_background(async move { reading.snapshot_page(&tess, after, limit, slices).await }).await?
    } else {
        (Vec::new(), None)
    };
    let documents = documents.into_iter().map(|(doc, _)| doc).collect();
    Ok(Json(hexdb_core::replication::SlicePage { source_id, applied_seq, documents, next }).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeParams {
    pub after: u64,
    pub wait: Option<u64>,
    pub limit: Option<usize>,
    /// The polling replica's hex ID; `after` doubles as its acknowledgement.
    pub hex: Option<String>,
    /// group (default) or system: see hexdb_core::replication::FeedScope.
    pub scope: Option<String>,
}

/// Changes after a sequence number, with typed documents; long-polls up to `wait` seconds.
pub async fn changes(
    params: Result<Query<ChangeParams>, QueryRejection>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    axum::extract::State(engine): Engine,
) -> ApiResult {
    authorize(&engine, &headers, &method, &uri, b"")?;
    not_rebuilding(&engine)?;
    let Query(params) = params?;
    let scope = feed_scope(&engine, params.scope.as_deref())?;
    let limit = params.limit.unwrap_or(1000).clamp(1, 10_000);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(params.wait.unwrap_or(0).min(30));
    let gone = |e: hexdb_core::HistoryExpired| {
        ApiError::new(
            StatusCode::GONE,
            "history_expired",
            format!("Changes after {} are no longer kept; take a new snapshot.", e.available_after),
        )
    };
    // A group's feed is for its own hexes: one that's now in another partition must not
    // apply this partition's changes (it would hold documents that aren't its own).
    if let Some(hex) = params.hex.as_deref().filter(|_| scope == FeedScope::Group) {
        if !engine.in_my_group(hex) {
            return Err(ApiError::new(StatusCode::CONFLICT, "not_in_group", "That hex is in another partition now; it follows its own partition's Overseer."));
        }
    }
    // Only a group's replicas acknowledge its writes (replication.min_acks).
    if let Some(hex) = params.hex.as_deref().filter(|_| scope == FeedScope::Group) {
        engine.record_replica_progress(hex, params.after);
    }
    // A replica further behind than the in-memory feed reads the history on disk.
    if params.after < engine.changes.available_after() {
        let taken = engine.changes_after(params.after, limit).await.map_err(gone)?;
        let last_seq = taken.last().map_or(params.after, |c| c.seq);
        let changes = taken.iter().filter(|c| engine.in_feed(&c.tessellation, scope)).map(|c| (**c).clone()).collect();
        still_overseer(&engine)?;
        return Ok(Json(ChangeBatch {
            source_id: engine.history_id(),
            changes,
            last_seq,
            published_seq: engine.changes.published_seq(),
            catalog: engine.catalog_fingerprint(),
            pending_since: engine.change_time_after(last_seq).await,
        })
        .into_response());
    }
    loop {
        let (backlog, mut receiver) = engine.changes.follow(params.after).map_err(gone)?;
        if !backlog.is_empty() || tokio::time::Instant::now() >= deadline {
            // Whole commits only, so the replica applies each transaction whole.
            let taken: Vec<_> = match hexdb_core::changes::whole_commits(backlog.clone(), limit) {
                Some(taken) => taken,
                None => {
                    let end = backlog.first().map(|c| c.commit_end()).unwrap_or(params.after);
                    backlog.into_iter().take_while(|c| c.seq <= end).collect()
                }
            };
            let last_seq = taken.last().map_or(params.after, |c| c.seq);
            let changes = taken
                .iter()
                .filter(|c| engine.in_feed(&c.tessellation, scope))
                .map(|c| (**c).clone())
                .collect();
            still_overseer(&engine)?;
            return Ok(Json(ChangeBatch {
                source_id: engine.history_id(),
                changes,
                last_seq,
                published_seq: engine.changes.published_seq(),
                catalog: engine.catalog_fingerprint(),
                pending_since: engine.change_time_after(last_seq).await,
            })
            .into_response());
        }
        let _ = tokio::time::timeout_at(deadline, receiver.recv()).await;
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revocation {
    pub session_id: String,
    pub expires_at: i64,
}

/// Record a sign-out made on a replica (replicas can't write).
pub async fn revoke(method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine, body: Bytes) -> ApiResult {
    authorize(&engine, &headers, &method, &uri, &body)?;
    let input: Revocation = serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    hexdb_core::auth::revoke_session(&engine, &input.session_id, input.expires_at).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThrottleUpdate {
    #[serde(default)]
    pub fail: Vec<String>,
    #[serde(default)]
    pub clear: Vec<String>,
}

/// Record sign-in failures (or a success) seen on a replica.
pub async fn throttle(method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine, body: Bytes) -> ApiResult {
    authorize(&engine, &headers, &method, &uri, &body)?;
    let input: ThrottleUpdate = serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    if input.fail.len() > 4 || input.clear.len() > 4 {
        return Err(ApiError::invalid("Too many keys."));
    }
    let fail: Vec<&str> = input.fail.iter().map(String::as_str).collect();
    let clear: Vec<&str> = input.clear.iter().map(String::as_str).collect();
    hexdb_core::auth::record_throttle(&engine, &fail, &clear).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Run a discovery round now: another hex of the lattice asks when the
/// candidates change (an Overseer handing its role over), so the election
/// doesn't wait for the next scheduled round. Any hex answers; a burst of asks
/// runs one round.
pub async fn elect(method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine, body: Bytes) -> ApiResult {
    use std::sync::atomic::{AtomicI64, Ordering};
    static LAST: AtomicI64 = AtomicI64::new(0);
    verify_signature(&engine, &headers, &method, &uri, &body)?;
    let now = jiff::Timestamp::now().as_millisecond();
    if now - LAST.swap(now, Ordering::Relaxed) >= 300 {
        tokio::spawn(async move { hexdb_core::network::discovery::elect_now(&engine).await });
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Store an audit event recorded on a replica.
pub async fn audit(method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine, body: Bytes) -> ApiResult {
    authorize(&engine, &headers, &method, &uri, &body)?;
    let event: hexdb_core::AuditEvent = serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    engine.store_audit(&event).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Add AI usage recorded on a replica to the month's total.
pub async fn ai_usage(method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine, body: Bytes) -> ApiResult {
    authorize(&engine, &headers, &method, &uri, &body)?;
    let entry: hexdb_core::ai::UsageEntry = serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    engine.store_ai_usage(&entry).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// A replica's content position (see `hexdb_core::engine::files::content`):
/// it holds the content of every file up to `seq` of this hex's history.
pub async fn content(method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine, body: Bytes) -> ApiResult {
    authorize(&engine, &headers, &method, &uri, &body)?;
    let body: serde_json::Value = serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    let (hex, source, seq) = (body["hex"].as_str(), body["source_id"].as_str(), body["seq"].as_u64());
    let (Some(hex), Some(source), Some(seq)) = (hex, source, seq) else {
        return Err(ApiError::invalid("hex, source_id and seq are required."));
    };
    engine.record_content_progress(hex, source, seq);
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Re-hash an API key with the current lattice key, asked by a replica that
/// verified it with a previous one (see hexdb_core::auth::authenticate).
pub async fn rehash_key(method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine, body: Bytes) -> ApiResult {
    authorize(&engine, &headers, &method, &uri, &body)?;
    let body: serde_json::Value = serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    let field = |k: &str| body[k].as_str().ok_or_else(|| ApiError::invalid(format!("{} is required.", k)));
    let id = ulid::Ulid::from_string(field("id")?).map_err(|_| ApiError::invalid("id isn't an API key ID."))?;
    // Whether the new hash is stored now: a replica asks again until it is.
    let stored = hexdb_core::auth::store_rehashed_key(&engine, id, field("old_hash")?, field("new_hash")?, field("key")?).await?;
    Ok(Json(serde_json::json!({ "stored": stored })).into_response())
}

/// `POST /lattice/first-password` `{"user", "old_hash", "new_hash"}`: a replica's sign-in
/// chose the new password a one-time password requires; `{"changed": true}` once stored,
/// false if the password changed meanwhile or no longer needs changing.
pub async fn first_password(method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine, body: Bytes) -> ApiResult {
    authorize(&engine, &headers, &method, &uri, &body)?;
    let body: serde_json::Value = serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    let field = |k: &str| body[k].as_str().ok_or_else(|| ApiError::invalid(format!("{} is required.", k)));
    let changed = hexdb_core::users::complete_required_password_change(&engine, field("user")?, field("old_hash")?, field("new_hash")?).await?;
    Ok(Json(serde_json::json!({ "changed": changed })).into_response())
}

/// `POST /lattice/mfa-step` `{"user", "step"}`: a replica's sign-in used this TOTP step;
/// `{"used": true}` if it's the first use (it's recorded), false if it was used before.
pub async fn mfa_step(method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine, body: Bytes) -> ApiResult {
    authorize(&engine, &headers, &method, &uri, &body)?;
    let body: serde_json::Value = serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    let id = body["user"].as_str().and_then(|s| ulid::Ulid::from_string(s).ok()).ok_or_else(|| ApiError::invalid("user is required."))?;
    let step = body["step"].as_i64().ok_or_else(|| ApiError::invalid("step is required."))?;
    let used = hexdb_core::users::consume_mfa_step(&engine, id, step).await?;
    Ok(Json(serde_json::json!({ "used": used })).into_response())
}

/// Count a sharing link download made on a replica.
pub async fn share_download(method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine, body: Bytes) -> ApiResult {
    authorize(&engine, &headers, &method, &uri, &body)?;
    let body: serde_json::Value = serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    let code = body["code"].as_str().ok_or_else(|| ApiError::invalid("code is required."))?;
    // Hexes before byte counting send no fraction: a whole download.
    let fraction = body["fraction"].as_f64().unwrap_or(1.0);
    engine.store_share_download(code, fraction).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Ask the Overseer to record a sign-out made on this replica.
pub async fn forward_revocation(engine: &HexDBEngine, session_id: &str, expires_at: i64) -> Result<(), ApiError> {
    let body = serde_json::json!({ "session_id": session_id, "expires_at": expires_at });
    hexdb_core::replication::post_to_overseer(engine, "/lattice/revoke", &body).await.map_err(|e| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "overseer_unreachable",
            format!("Couldn't record the sign-out with the Overseer: {:#}. Try again shortly.", e),
        )
    })
}

// ---------------------------------------------------------------------------
// The hosting control service (HexDB Enterprise)
// ---------------------------------------------------------------------------
//
//   GET  /lattice/control/hexes    every hex: machine, region, version, position, retiring, removable
//   POST /lattice/control/retire   {"hex": "<machine ID, name or ID>", "cancel"?: bool}
//
// For a hosted lattice's control service, which adds and removes its machines
// when the plan changes. Authorized by `updates.control_token` (the token the
// lattice itself presents to `updates.control_url`) as a Bearer token; off on
// lattices that aren't hosted. Any hex answers; a retirement reaching a
// replica goes on to the Overseer as a signed lattice request.

/// The control token, checked in constant time.
fn control_authorized(engine: &HexDBEngine, headers: &HeaderMap) -> Result<(), ApiError> {
    let config = &engine.config.updates;
    if !config.hosted() {
        return Err(ApiError::not_found("This lattice isn't hosted."));
    }
    let given = headers.get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or_default();
    let expected = config.control_token.trim();
    let same = given.len() == expected.len() && given.bytes().zip(expected.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
    if !same {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "Needs the lattice's control token."));
    }
    Ok(())
}

/// `GET /lattice/control/hexes`.
pub async fn control_hexes(headers: HeaderMap, axum::extract::State(engine): Engine) -> ApiResult {
    control_authorized(&engine, &headers)?;
    Ok(Json(engine.control_view().await).into_response())
}

#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlRetire {
    hex: String,
    #[serde(default)]
    cancel: bool,
}

async fn apply_retire(engine: &HexDBEngine, request: &ControlRetire) -> ApiResult {
    if request.cancel {
        engine.unretire_hex(&request.hex, "control").await?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let view = engine.retire_hex(&request.hex, "control").await?;
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!(view))).into_response())
}

/// `POST /lattice/control/retire`: here on the Overseer, or passed on to it.
pub async fn control_retire(headers: HeaderMap, axum::extract::State(engine): Engine, body: Bytes) -> ApiResult {
    control_authorized(&engine, &headers)?;
    let request: ControlRetire = serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    if engine.is_writable() {
        return apply_retire(&engine, &request).await;
    }
    let value = serde_json::to_value(&request).map_err(|e| ApiError::invalid(e.to_string()))?;
    tokio::time::timeout(Duration::from_secs(15), hexdb_core::replication::post_to_overseer(&engine, "/lattice/retire", &value))
        .await
        .map_err(|_| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "overseer_unreachable", "The Overseer didn't answer in time."))?
        .map_err(|e| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "overseer_unreachable", format!("{:#}", e)))?;
    Ok(StatusCode::ACCEPTED.into_response())
}

/// `POST /lattice/retire`: a retirement a replica passed on (signed; on the Overseer).
pub async fn retire(method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine, body: Bytes) -> ApiResult {
    authorize(&engine, &headers, &method, &uri, &body)?;
    let request: ControlRetire = serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    apply_retire(&engine, &request).await
}
