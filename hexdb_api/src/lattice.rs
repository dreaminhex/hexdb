// HexDB API: lattice replication endpoints (hex-to-hex)
//
//   GET /lattice/snapshot                 where a full sync starts (sequence, catalog)
//   GET /lattice/catalog                  tessellations and index definitions
//   GET /lattice/snapshot/{tessellation}  documents with versions, paged by ID
//   GET /lattice/changes?after=&wait=     the change feed with typed documents
//
// Every request needs the `X-HexDB-Lattice-Token` header (see
// `hexdb_core::replication`), and only the Overseer serves them.

use crate::handlers::{ApiError, ApiResult, Engine};
use axum::{
    extract::{rejection::QueryRejection, Path, Query},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use hexdb_core::{
    constant_time_eq, engine::HexDBEngine, lattice_token, replication::{ChangeBatch, SnapshotPage}, REPLICATION_TESSELLATION,
    LATTICE_TOKEN_HEADER,
};
use serde::Deserialize;
use std::time::Duration;
use ulid::Ulid;

fn authorize(engine: &HexDBEngine, headers: &HeaderMap) -> Result<(), ApiError> {
    let presented = headers.get(LATTICE_TOKEN_HEADER).and_then(|v| v.to_str().ok()).unwrap_or_default();
    if !constant_time_eq(presented.as_bytes(), lattice_token(&engine.config).as_bytes()) {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "A valid lattice token is required."));
    }
    if !engine.is_writable() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "not_overseer",
            format!("This hex is a {}; replicate from the Overseer.", engine.role()),
        ));
    }
    Ok(())
}

/// Where a full sync starts.
pub async fn snapshot(headers: HeaderMap, axum::extract::State(engine): Engine) -> ApiResult {
    authorize(&engine, &headers)?;
    Ok(Json(engine.snapshot_meta()).into_response())
}

/// Tessellations and index definitions (replicas poll this for new ones).
pub async fn catalog(headers: HeaderMap, axum::extract::State(engine): Engine) -> ApiResult {
    authorize(&engine, &headers)?;
    Ok(Json(engine.snapshot_meta()).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageParams {
    pub after: Option<String>,
    pub limit: Option<usize>,
}

/// One page of a tessellation's documents with their versions.
pub async fn snapshot_page(
    Path(tess): Path<String>,
    params: Result<Query<PageParams>, QueryRejection>,
    headers: HeaderMap,
    axum::extract::State(engine): Engine,
) -> ApiResult {
    authorize(&engine, &headers)?;
    let Query(params) = params?;
    if !engine.tessellation_exists(&tess) {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found", format!("Tessellation '{}' not found.", tess)));
    }
    let after = match params.after.as_deref() {
        Some(a) => Some(Ulid::from_string(a).map_err(|_| ApiError::invalid("after must be a document ID."))?),
        None => None,
    };
    let (documents, next) = engine.snapshot_page(&tess, after, params.limit.unwrap_or(500).clamp(1, 5000)).await?;
    Ok(Json(SnapshotPage { documents, next }).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeParams {
    pub after: u64,
    pub wait: Option<u64>,
    pub limit: Option<usize>,
}

/// Changes after a sequence number, with typed documents; long-polls up to `wait` seconds.
pub async fn changes(
    params: Result<Query<ChangeParams>, QueryRejection>,
    headers: HeaderMap,
    axum::extract::State(engine): Engine,
) -> ApiResult {
    authorize(&engine, &headers)?;
    let Query(params) = params?;
    let limit = params.limit.unwrap_or(1000).clamp(1, 10_000);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(params.wait.unwrap_or(0).min(30));
    loop {
        let (backlog, mut receiver) = engine.changes.follow(params.after).map_err(|e| {
            ApiError::new(
                StatusCode::GONE,
                "history_expired",
                format!("Changes after {} are no longer kept; take a new snapshot.", e.available_after),
            )
        })?;
        if !backlog.is_empty() || tokio::time::Instant::now() >= deadline {
            let taken: Vec<_> = backlog.into_iter().take(limit).collect();
            let last_seq = taken.last().map_or(params.after, |c| c.seq);
            let changes = taken
                .iter()
                .filter(|c| c.tessellation != REPLICATION_TESSELLATION)
                .map(|c| (**c).clone())
                .collect();
            return Ok(Json(ChangeBatch {
                source_id: engine.id.to_string(),
                changes,
                last_seq,
                published_seq: engine.changes.published_seq(),
            })
            .into_response());
        }
        let _ = tokio::time::timeout_at(deadline, receiver.recv()).await;
    }
}
