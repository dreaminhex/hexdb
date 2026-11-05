// HexDB API: quarantined uploads (files the malware scan flagged).
//
// Administrators only. Quarantine lives on the Overseer, which scans every upload,
// so these requests are forwarded there from a replica (see forward.rs).

use crate::auth::Auth;
use crate::handlers::{ApiResult, Engine};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde_json::json;

/// `GET /quarantine`: quarantined files, newest first, with what they matched.
pub async fn list(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    let files = engine.quarantined_files().await?;
    Ok(Json(json!({ "files": files })).into_response())
}

/// `POST /quarantine/{id}/release`: store a quarantined file in its tessellation
/// after all (a false positive). Audited.
pub async fn release(Path(id): Path<String>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    let result = engine.release_quarantined(&id, &principal.login).await?;
    Ok(Json(json!({ "document": result.document.to_api_json(), "created": result.created })).into_response())
}

/// `POST /quarantine/rescan`: scan every stored file again now (202; see `/status`
/// `storage.scan.rescan`), e.g. after adding YARA rules.
pub async fn rescan(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    engine.request_rescan()?;
    engine.audit(&principal.login, "file.rescan_requested", "files", json!({})).await;
    Ok((StatusCode::ACCEPTED, Json(json!({ "rescan": "started" }))).into_response())
}

/// `DELETE /quarantine/{id}`: delete a quarantined file for good. Audited.
pub async fn delete(Path(id): Path<String>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    engine.delete_quarantined(&id, &principal.login).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
