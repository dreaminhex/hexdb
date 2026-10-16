// HexDB API: sharing links (see hexdb_core::shares)
//
// For signed-in users with the `share` permission:
//   POST   /shares              {"tessellation", "file_id" | "folder", "expires_in_days"?, "password"?, "max_downloads"?, "note"?}
//   GET    /shares              your links (?all=true: everyone's, administrators)
//   DELETE /shares/{code}       delete a link (its maker or an administrator)
//
// For anyone with a link (no account):
//   GET  /_s/{code}              what the link is (browsers are sent to the admin UI's page for it)
//   POST /_s/{code}/unlock       {"password"} -> {"token"}, for password-protected links (an hour)
//   GET  /_s/{code}/content      the file (?file=<id> in a folder link; ?t=<token>; ?download=true)
//   GET  /_s/{code}/thumbnail    its thumbnail (same parameters)
//   GET  /_s/{code}/files        a folder link's files
//
// Every public request resolves the link and its maker again, so a deleted,
// expired or exhausted link, or a maker who lost access, stops it at once.
// Unknown and dead codes get the same 404; they and wrong passwords count
// toward a per-address lockout.

use crate::auth::Auth;
use crate::files::{serve, ContentParams};
use crate::handlers::{ApiError, ApiResult, Engine};
use axum::{
    extract::{rejection::JsonRejection, Path, Query},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use hexdb_core::{engine::HexDBEngine, filter::Filter, shares::Share, AuditEvent, Principal};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

fn gone() -> ApiError {
    ApiError::not_found("This link doesn't exist or has expired.")
}

/// `POST /shares`.
pub async fn create(
    axum::extract::State(engine): Engine,
    Auth(principal): Auth,
    crate::client::ClientIp(client): crate::client::ClientIp,
    body: Result<Json<hexdb_core::shares::NewShare>, JsonRejection>,
) -> ApiResult {
    let Json(input) = body?;
    let share = engine.create_share(&principal, input).await?;
    engine
        .record_audit(
            AuditEvent::new(&principal.login, "share.create", &share.name)
                .client(&client)
                .details(json!({ "code": share.code, "tessellation": share.tessellation, "file_id": share.file_id, "folder": share.folder, "expires_at": share.expires_at, "password": share.password_hash.is_some(), "max_downloads": share.max_downloads })),
        )
        .await;
    Ok((StatusCode::CREATED, Json(share.view())).into_response())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    #[serde(default)]
    pub all: bool,
}

/// `GET /shares`.
pub async fn list(axum::extract::State(engine): Engine, Auth(principal): Auth, params: Query<ListParams>) -> ApiResult {
    let shares: Vec<Value> = engine.list_shares(&principal, params.all).await?.iter().map(Share::view).collect();
    Ok(Json(json!({ "shares": shares })).into_response())
}

/// `DELETE /shares/{code}`.
pub async fn delete(Path(code): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth, crate::client::ClientIp(client): crate::client::ClientIp) -> ApiResult {
    let share = engine.delete_share(&principal, &code).await?;
    engine.record_audit(AuditEvent::new(&principal.login, "share.delete", &share.name).client(&client).details(json!({ "code": code }))).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Public
// ---------------------------------------------------------------------------

/// The link and its maker, or the same 404 for anything that isn't a live link.
async fn open(engine: &HexDBEngine, code: &str, client: &str) -> Result<(Share, Principal), ApiError> {
    engine.share_attempt_allowed(client).await?;
    if engine.config_value("sharing.enabled") == json!(false) {
        return Err(gone());
    }
    let Some(share) = engine.share(code).await?.filter(|s| !s.expired()) else {
        engine.share_attempt_failed(client).await;
        return Err(gone());
    };
    let principal = engine.share_principal(&share).await?.ok_or_else(gone)?;
    Ok((share, principal))
}

/// Refuse a password-protected link without a valid unlock token.
fn unlocked(engine: &HexDBEngine, share: &Share, token: Option<&str>) -> Result<(), ApiError> {
    if share.password_hash.is_none() || token.is_some_and(|t| engine.share_token_valid(&share.code, t)) {
        Ok(())
    } else {
        Err(ApiError::new(StatusCode::UNAUTHORIZED, "password_required", "This link needs its password."))
    }
}

/// The file a request means: the link's own file, or one under its folder (as its maker sees it).
async fn target(engine: &HexDBEngine, share: &Share, file: Option<&str>) -> Result<String, ApiError> {
    match (&share.file_id, &share.folder) {
        (Some(id), _) => Ok(id.clone()),
        (None, Some(folder)) => {
            let id = file.ok_or_else(|| ApiError::invalid("Say which file (?file=<id>)."))?;
            let content = engine.file_content(&share.tessellation, id, None).await.map_err(|_| gone())?;
            let (doc, _) = engine.get_document_versioned(&share.tessellation, &content.id.to_string()).await?.ok_or_else(gone)?;
            let path = doc.data_json()["path"].as_str().unwrap_or_default().to_string();
            if !path.starts_with(folder.as_str()) {
                return Err(gone());
            }
            Ok(content.id.to_string())
        }
        _ => Err(gone()),
    }
}

fn no_index(mut response: Response) -> Response {
    response.headers_mut().insert(header::HeaderName::from_static("x-robots-tag"), HeaderValue::from_static("noindex, nofollow"));
    response
}

/// `GET /_s/{code}`.
pub async fn info(Path(code): Path<String>, axum::extract::State(engine): Engine, crate::client::ClientIp(client): crate::client::ClientIp, headers: HeaderMap) -> ApiResult {
    // A browser following the link gets the page that shows it.
    let wants_html = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.contains("text/html"));
    if wants_html && hexdb_core::shares::code_id(&code).is_some() {
        let mut response = StatusCode::SEE_OTHER.into_response();
        if let Ok(location) = HeaderValue::from_str(&format!("/ui/s/{}", code)) {
            response.headers_mut().insert(header::LOCATION, location);
        }
        return Ok(no_index(response));
    }
    let (share, principal) = open(&engine, &code, &client).await?;
    let mut view = share.public_view();
    if share.password_hash.is_none() {
        if let Some(id) = &share.file_id {
            let content = hexdb_core::access::as_caller(Arc::new(principal), engine.file_content(&share.tessellation, id, None)).await.map_err(|_| gone())?;
            view["file"] = json!({ "id": id, "name": content.name, "size": content.size, "content_type": content.content_type });
        }
    }
    Ok(no_index(Json(view).into_response()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnlockBody {
    pub password: String,
}

/// `POST /_s/{code}/unlock`.
pub async fn unlock(Path(code): Path<String>, axum::extract::State(engine): Engine, crate::client::ClientIp(client): crate::client::ClientIp, body: Result<Json<UnlockBody>, JsonRejection>) -> ApiResult {
    let Json(body) = body?;
    let (share, _) = open(&engine, &code, &client).await?;
    let Some(hash) = &share.password_hash else {
        return Ok(Json(json!({ "token": null })).into_response());
    };
    if body.password.len() > 1024 || !hexdb_core::crypt::verify_hash(&body.password, hash) {
        engine.share_attempt_failed(&client).await;
        engine.record_audit(AuditEvent::new("-", "share.unlock", &share.name).outcome("failed").client(&client).details(json!({ "code": code }))).await;
        return Err(ApiError::new(StatusCode::FORBIDDEN, "wrong_password", "That password isn't right."));
    }
    Ok(no_index(Json(json!({ "token": engine.share_unlock_token(&code)?, "expires_in": hexdb_core::shares::UNLOCK_MINUTES * 60 })).into_response()))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicParams {
    pub file: Option<String>,
    pub t: Option<String>,
    #[serde(default)]
    pub download: bool,
    pub path: Option<String>,
}

/// `GET /_s/{code}/content`.
pub async fn content(
    Path(code): Path<String>,
    axum::extract::State(engine): Engine,
    crate::client::ClientIp(client): crate::client::ClientIp,
    params: Query<PublicParams>,
    method: Method,
    headers: HeaderMap,
) -> ApiResult {
    let (share, principal) = open(&engine, &code, &client).await?;
    unlocked(&engine, &share, params.t.as_deref())?;
    let caller = Arc::new(principal);
    let id = hexdb_core::access::as_caller(caller.clone(), target(&engine, &share, params.file.as_deref())).await?;
    let response = hexdb_core::access::as_caller(caller, serve(engine.clone(), &share.tessellation, &id, ContentParams { version: None, download: params.download }, &method, &headers)).await?;
    // A download is a GET that starts at the beginning of the file.
    let from_start = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).is_none_or(|r| r.trim_start_matches("bytes=").starts_with("0-"));
    if method == Method::GET && response.status().is_success() && from_start {
        engine.count_share_download(&code).await;
        engine
            .record_audit(AuditEvent::new("-", "share.download", &share.name).client(&client).details(json!({ "code": code, "file_id": id, "shared_by": share.created_by })))
            .await;
    }
    Ok(no_index(response))
}

/// `GET /_s/{code}/thumbnail`.
pub async fn thumbnail(Path(code): Path<String>, axum::extract::State(engine): Engine, crate::client::ClientIp(client): crate::client::ClientIp, params: Query<PublicParams>) -> ApiResult {
    let (share, principal) = open(&engine, &code, &client).await?;
    unlocked(&engine, &share, params.t.as_deref())?;
    let caller = Arc::new(principal);
    let id = hexdb_core::access::as_caller(caller.clone(), target(&engine, &share, params.file.as_deref())).await?;
    let (bytes, content_type, _) = hexdb_core::access::as_caller(caller, engine.file_thumbnail(&share.tessellation, &id)).await.map_err(|_| ApiError::not_found("No thumbnail."))?;
    let mut response = (StatusCode::OK, bytes.to_vec()).into_response();
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=300"));
    Ok(no_index(response))
}

/// `GET /_s/{code}/files`: the files under a folder link (as its maker sees them), at most 1,000.
pub async fn files(Path(code): Path<String>, axum::extract::State(engine): Engine, crate::client::ClientIp(client): crate::client::ClientIp, params: Query<PublicParams>) -> ApiResult {
    let (share, principal) = open(&engine, &code, &client).await?;
    unlocked(&engine, &share, params.t.as_deref())?;
    let Some(folder) = &share.folder else {
        return Err(ApiError::invalid("This link is to one file."));
    };
    let filter = Filter::parse(&json!({ "path": { "$startsWith": folder } }))?;
    let query = hexdb_core::engine::DocumentQuery { filter, sort: vec![hexdb_core::filter::SortKey { field: "path".into(), descending: false }], offset: 0, limit: 1000, after: None, with_total: true };
    let page = hexdb_core::access::as_caller(Arc::new(principal), engine.query_documents(&share.tessellation, &query)).await?;
    let files: Vec<Value> = page
        .documents
        .iter()
        .map(|d| {
            let data = d.data_json();
            let path = data["path"].as_str().unwrap_or_default();
            json!({
                "id": d.id.to_string(),
                "name": data["name"],
                "folder": format!("/{}", &path[folder.len().min(path.len())..]),
                "size": data["size"],
                "content_type": data["content_type"],
                "modified_at": data["modified_at"],
                "thumbnail": data.get("thumbnail").is_some(),
            })
        })
        .collect();
    Ok(no_index(Json(json!({ "name": share.name, "files": files, "total": page.total })).into_response()))
}
