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
    replication::{ChangeBatch, SnapshotPage},
    LATTICE_SIGNATURE_HEADER, REPLICATION_TESSELLATION,
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
    let signature = if request.uri().path().starts_with("/lattice/") {
        request.headers().get(LATTICE_SIGNATURE_HEADER).and_then(|v| v.to_str().ok()).map(String::from)
    } else {
        None
    };
    let response = next.run(request).await;
    let Some(signature) = signature else { return response };
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

/// A signed request to an endpoint only the Overseer serves.
fn authorize(engine: &HexDBEngine, headers: &HeaderMap, method: &Method, uri: &Uri, body: &[u8]) -> Result<(), ApiError> {
    verify_signature(engine, headers, method, uri, body)?;
    if !engine.is_writable() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "not_overseer",
            format!("This hex is a {}; replicate from the Overseer.", engine.role()),
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
    let data = engine.read_local_chunk(id).await.map_err(|_| missing())?;
    let compressed = engine.blobs().is_compressed(&id).unwrap_or(false);
    let sealed = engine.lattice_keys.seal(&data, &id);
    let mut response = sealed.into_response();
    response.headers_mut().insert("x-hexdb-compress", axum::http::HeaderValue::from_static(if compressed { "1" } else { "0" }));
    Ok(response)
}

/// Where a full sync starts.
pub async fn snapshot(method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine) -> ApiResult {
    authorize(&engine, &headers, &method, &uri, b"")?;
    not_rebuilding(&engine)?;
    Ok(Json(engine.snapshot_meta()).into_response())
}

/// Tessellations and index definitions (replicas poll this for new ones).
pub async fn catalog(method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine) -> ApiResult {
    authorize(&engine, &headers, &method, &uri, b"")?;
    not_rebuilding(&engine)?;
    Ok(Json(engine.snapshot_meta()).into_response())
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
    let (documents, next) = engine.snapshot_page(&tess, after, params.limit.unwrap_or(500).clamp(1, 5000), slices).await?;
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
        engine.snapshot_page(&tess, after, params.limit.unwrap_or(500).clamp(1, 5000), slices).await?
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
    let limit = params.limit.unwrap_or(1000).clamp(1, 10_000);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(params.wait.unwrap_or(0).min(30));
    let gone = |e: hexdb_core::HistoryExpired| {
        ApiError::new(
            StatusCode::GONE,
            "history_expired",
            format!("Changes after {} are no longer kept; take a new snapshot.", e.available_after),
        )
    };
    if let Some(hex) = params.hex.as_deref() {
        engine.record_replica_progress(hex, params.after);
    }
    // A replica further behind than the in-memory feed reads the history on disk.
    if params.after < engine.changes.available_after() {
        let taken = engine.changes_after(params.after, limit).await.map_err(gone)?;
        let last_seq = taken.last().map_or(params.after, |c| c.seq);
        let changes = taken.iter().filter(|c| c.tessellation != REPLICATION_TESSELLATION).map(|c| (**c).clone()).collect();
        return Ok(Json(ChangeBatch {
            source_id: engine.history_id(),
            changes,
            last_seq,
            published_seq: engine.changes.published_seq(),
            catalog: engine.catalog_fingerprint(),
        })
        .into_response());
    }
    loop {
        let (backlog, mut receiver) = engine.changes.follow(params.after).map_err(gone)?;
        if !backlog.is_empty() || tokio::time::Instant::now() >= deadline {
            let taken: Vec<_> = backlog.into_iter().take(limit).collect();
            let last_seq = taken.last().map_or(params.after, |c| c.seq);
            let changes = taken
                .iter()
                .filter(|c| c.tessellation != REPLICATION_TESSELLATION)
                .map(|c| (**c).clone())
                .collect();
            return Ok(Json(ChangeBatch {
                source_id: engine.history_id(),
                changes,
                last_seq,
                published_seq: engine.changes.published_seq(),
                catalog: engine.catalog_fingerprint(),
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
