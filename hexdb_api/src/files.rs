// HexDB HTTP handlers for files tessellations (see hexdb_core::files).
//
//   PUT    /{tess}/_files/{folder/name}       upload (raw body); a new version if the path exists
//   GET    /{tess}/_files/{folder/name}       download by path (also HEAD)
//   GET    /{tess}/{id}/content               download (Range, ETag, ?version=, ?download=true)
//   GET    /{tess}/{id}/thumbnail             a small JPEG/PNG of an image (when one was made)
//   GET    /{tess}/{id}/versions              the current and earlier versions
//   POST   /{tess}/{id}/versions/{n}/restore  make version n current again
//   GET    /{tess}/_trash                     deleted files
//   DELETE /{tess}/_trash                     empty the trash
//   POST   /{tess}/_trash/{id}/restore        undelete (?name=, ?path= to avoid a clash)
//   DELETE /{tess}/_trash/{id}                purge one file
//   POST   /{tess}/_reextract                 read metadata again (?id= for one file)
//
// Resumable uploads, for large files and unreliable connections:
//   POST   /{tess}/_uploads                   {"path", "size"?, "content_type"?, "modified"?, "created"?} -> {"id", "offset"}
//   PATCH  /{tess}/_uploads/{id}?offset=N      the next part (raw body), starting at byte N
//   GET    /{tess}/_uploads/{id}              how far it got
//   POST   /{tess}/_uploads/{id}/complete     create the file (or its new version)
//   DELETE /{tess}/_uploads/{id}              abandon it (sessions also expire after a day)
//
// A file's metadata is its document: GET /{tess}/{id}, queries, GraphQL and
// SQL work as for any tessellation. DELETE /{tess}/{id} moves it to the trash.

use crate::auth::Auth;
use crate::handlers::{docs_json, no_content, respond, user_tessellation, with_location, ApiError, ApiResult, Engine};
use axum::{
    body::{Body, Bytes},
    extract::{rejection::QueryRejection, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use hexdb_core::{engine::HexDBEngine, engine::UploadOptions, Permission};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Debug, Default, Deserialize)]
pub struct UploadParams {
    /// When the file was last modified (RFC 3339 or epoch milliseconds).
    pub modified: Option<String>,
    /// When the file was created (RFC 3339 or epoch milliseconds).
    pub created: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ContentParams {
    /// An earlier version's number.
    pub version: Option<u64>,
    /// Always download (Content-Disposition: attachment), even for types a browser can show.
    #[serde(default)]
    pub download: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewUpload {
    pub path: String,
    pub size: Option<u64>,
    pub content_type: Option<String>,
    pub modified: Option<String>,
    pub created: Option<String>,
}

/// Start a resumable upload.
pub async fn begin_upload(
    Path(tess): Path<String>,
    State(engine): Engine,
    Auth(principal): Auth,
    body: Result<Json<NewUpload>, axum::extract::rejection::JsonRejection>,
) -> ApiResult {
    principal.require(Permission::Upload, &tess)?;
    let Json(input) = body?;
    user_tessellation(&engine, &tess)?;
    let options = UploadOptions {
        content_type: input.content_type,
        modified_at: input.modified.as_deref().map(|t| parse_time(t, "modified")).transpose()?,
        created_at: input.created.as_deref().map(|t| parse_time(t, "created")).transpose()?,
    };
    let session = engine.begin_upload(&tess, &input.path, options, input.size).await?;
    let location = format!("/{}/_uploads/{}", tess, session["id"].as_str().unwrap_or_default());
    Ok(with_location(respond(StatusCode::CREATED, session, false), &location))
}

#[derive(Debug, Default, Deserialize)]
pub struct PartParams {
    pub offset: Option<u64>,
}

/// Send the next part of an upload. The offset comes from `?offset=` or the `Upload-Offset` header.
pub async fn upload_part(
    Path((tess, id)): Path<(String, String)>,
    params: Result<Query<PartParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
    headers: HeaderMap,
    body: Body,
) -> ApiResult {
    principal.require(Permission::Upload, &tess)?;
    let Query(params) = params?;
    user_tessellation(&engine, &tess)?;
    let offset = params
        .offset
        .or_else(|| headers.get("upload-offset").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok()))
        .ok_or_else(|| ApiError::invalid("Give the part's starting byte as ?offset= or an Upload-Offset header."))?;
    let session = engine.upload_part(&tess, &id, offset, body.into_data_stream()).await?;
    Ok(respond(StatusCode::OK, session, false))
}

/// How far an upload got.
pub async fn upload_status(Path((tess, id)): Path<(String, String)>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require(Permission::Upload, &tess)?;
    user_tessellation(&engine, &tess)?;
    Ok(Json(engine.upload_status(&tess, &id).await?).into_response())
}

/// Finish an upload.
pub async fn complete_upload(Path((tess, id)): Path<(String, String)>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require(Permission::Upload, &tess)?;
    user_tessellation(&engine, &tess)?;
    let result = engine.complete_upload(&tess, &id).await?;
    let status = if result.created { StatusCode::CREATED } else { StatusCode::OK };
    let doc_id = result.document.id;
    let mut response = respond(status, result.document.to_api_json(), false);
    if result.unchanged {
        response.headers_mut().insert("x-hexdb-unchanged", HeaderValue::from_static("true"));
    }
    Ok(with_location(response, &format!("/{}/{}", tess, doc_id)))
}

/// Abandon an upload.
pub async fn abort_upload(Path((tess, id)): Path<(String, String)>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require(Permission::Upload, &tess)?;
    user_tessellation(&engine, &tess)?;
    engine.abort_upload(&tess, &id).await?;
    Ok(no_content(false))
}

#[derive(Debug, Default, Deserialize)]
pub struct ReextractParams {
    pub id: Option<String>,
}

/// Queue files for metadata extraction again (all, or `?id=`).
pub async fn reextract(
    Path(tess): Path<String>,
    params: Result<Query<ReextractParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
) -> ApiResult {
    principal.require(Permission::Write, &tess)?;
    let Query(params) = params?;
    user_tessellation(&engine, &tess)?;
    let queued = engine.reextract_files(&tess, params.id.as_deref()).await?;
    Ok(respond(StatusCode::ACCEPTED, json!({ "queued": queued }), false))
}

#[derive(Debug, Default, Deserialize)]
pub struct RestoreParams {
    pub name: Option<String>,
    pub path: Option<String>,
}

fn parse_time(text: &str, what: &str) -> Result<DateTime<Utc>, ApiError> {
    if let Ok(millis) = text.parse::<i64>() {
        return DateTime::from_timestamp_millis(millis).ok_or_else(|| ApiError::invalid(format!("{}: out of range.", what)));
    }
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| ApiError::invalid(format!("{}: expected an RFC 3339 time or epoch milliseconds.", what)))
}

/// Upload a file's content to `folder/name`.
pub async fn upload(
    Path((tess, path)): Path<(String, String)>,
    params: Result<Query<UploadParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
    headers: HeaderMap,
    body: Body,
) -> ApiResult {
    principal.require(Permission::Upload, &tess)?;
    let Query(params) = params?;
    user_tessellation(&engine, &tess)?;
    let options = UploadOptions {
        content_type: headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(String::from),
        modified_at: params.modified.as_deref().map(|t| parse_time(t, "modified")).transpose()?,
        created_at: params.created.as_deref().map(|t| parse_time(t, "created")).transpose()?,
    };
    let result = engine.store_file(&tess, &path, options, body.into_data_stream()).await?;
    let status = if result.created { StatusCode::CREATED } else { StatusCode::OK };
    let id = result.document.id;
    let mut response = respond(status, result.document.to_api_json(), false);
    if result.unchanged {
        response.headers_mut().insert("x-hexdb-unchanged", HeaderValue::from_static("true"));
    }
    Ok(with_location(response, &format!("/{}/{}", tess, id)))
}

/// Download a file by its path.
pub async fn download_path(
    Path((tess, path)): Path<(String, String)>,
    params: Result<Query<ContentParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
    method: Method,
    headers: HeaderMap,
) -> ApiResult {
    principal.require(Permission::Read, &tess)?;
    let Query(params) = params?;
    user_tessellation(&engine, &tess)?;
    let doc = engine.file_at_path(&tess, &path).await?;
    serve(engine, &tess, &doc.id.to_string(), params, &method, &headers).await
}

/// Download a file's content by ID.
pub async fn download(
    Path((tess, id)): Path<(String, String)>,
    params: Result<Query<ContentParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
    method: Method,
    headers: HeaderMap,
) -> ApiResult {
    principal.require(Permission::Read, &tess)?;
    let Query(params) = params?;
    user_tessellation(&engine, &tess)?;
    serve(engine, &tess, &id, params, &method, &headers).await
}

/// Types a browser may show in place rather than download.
fn shown_inline(content_type: &str) -> bool {
    let t = content_type.to_ascii_lowercase();
    (t.starts_with("image/") && t != "image/svg+xml") || t.starts_with("audio/") || t.starts_with("video/") || t == "application/pdf" || t == "text/plain"
}

/// `Content-Disposition` with an ASCII fallback name and the UTF-8 name (RFC 6266).
fn disposition(kind: &str, name: &str) -> String {
    let fallback: String = name.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect();
    let mut encoded = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{:02X}", byte));
        }
    }
    format!("{}; filename=\"{}\"; filename*=UTF-8''{}", kind, fallback, encoded)
}

/// A single `bytes=` range, as (start, end exclusive). `Ok(None)`: the whole file.
fn parse_range(header: Option<&HeaderValue>, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(text) = header.and_then(|h| h.to_str().ok()) else { return Ok(None) };
    let Some(spec) = text.trim().strip_prefix("bytes=") else { return Ok(None) };
    if spec.contains(',') {
        // Several ranges: send the whole file (allowed by RFC 9110).
        return Ok(None);
    }
    let (first, last) = spec.split_once('-').ok_or(())?;
    let (first, last) = (first.trim(), last.trim());
    let range = if first.is_empty() {
        let suffix: u64 = last.parse().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        (size.saturating_sub(suffix), size)
    } else {
        let start: u64 = first.parse().map_err(|_| ())?;
        let end = if last.is_empty() { size } else { last.parse::<u64>().map_err(|_| ())?.saturating_add(1).min(size) };
        (start, end)
    };
    if range.0 >= size || range.0 >= range.1 {
        return Err(());
    }
    Ok(Some(range))
}

pub(crate) async fn serve(engine: Arc<HexDBEngine>, tess: &str, id: &str, params: ContentParams, method: &Method, headers: &HeaderMap) -> ApiResult {
    let content = engine.file_content(tess, id, params.version).await?;
    let etag = format!("\"{}\"", content.sha256);
    let mut response_headers = HeaderMap::new();
    let set = |h: &mut HeaderMap, name: header::HeaderName, value: String| {
        if let Ok(value) = HeaderValue::from_str(&value) {
            h.insert(name, value);
        }
    };
    set(&mut response_headers, header::ETAG, etag.clone());
    set(&mut response_headers, header::ACCEPT_RANGES, "bytes".into());
    set(&mut response_headers, header::CACHE_CONTROL, "private, no-cache".into());
    set(&mut response_headers, header::CONTENT_TYPE, content.content_type.clone());
    let kind = if !params.download && shown_inline(&content.content_type) { "inline" } else { "attachment" };
    set(&mut response_headers, header::CONTENT_DISPOSITION, disposition(kind, &content.name));
    if let Some(modified) = content.modified_at.as_deref().and_then(|t| DateTime::parse_from_rfc3339(t).ok()) {
        set(&mut response_headers, header::LAST_MODIFIED, modified.with_timezone(&Utc).format("%a, %d %b %Y %H:%M:%S GMT").to_string());
    }
    set(&mut response_headers, header::HeaderName::from_static("x-hexdb-version"), content.version.to_string());

    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*")) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response.headers_mut().extend(response_headers);
        return Ok(response);
    }

    // A Range applies only while the client's copy is current (If-Range).
    let if_range_ok = headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok()).is_none_or(|v| v.trim() == etag);
    let range = if if_range_ok { parse_range(headers.get(header::RANGE), content.size) } else { Ok(None) };
    let (status, start, end) = match range {
        Ok(Some((start, end))) => {
            set(&mut response_headers, header::CONTENT_RANGE, format!("bytes {}-{}/{}", start, end - 1, content.size));
            (StatusCode::PARTIAL_CONTENT, start, end)
        }
        Ok(None) => (StatusCode::OK, 0, content.size),
        Err(()) => {
            let mut response = ApiError::new(StatusCode::RANGE_NOT_SATISFIABLE, "range_not_satisfiable", "The requested range is outside the file.").into_response();
            set(response.headers_mut(), header::CONTENT_RANGE, format!("bytes */{}", content.size));
            return Ok(response);
        }
    };
    set(&mut response_headers, header::CONTENT_LENGTH, (end - start).to_string());

    let body = if *method == Method::HEAD || start == end {
        Body::empty()
    } else {
        let pieces = content.manifest.slice(start, end);
        let stream = futures::stream::unfold((engine, pieces.into_iter()), |(engine, mut pieces)| async move {
            let (chunk, range) = pieces.next()?;
            let item = engine
                .read_file_chunk(chunk)
                .await
                .map(|data| Bytes::copy_from_slice(&data[range]))
                .map_err(|e| std::io::Error::other(format!("{:#}", e)));
            Some((item, (engine, pieces)))
        });
        Body::from_stream(stream)
    };
    let mut response = (status, body).into_response();
    response.headers_mut().extend(response_headers);
    Ok(response)
}

/// A file's thumbnail (images and Photoshop documents, once extraction ran).
pub async fn thumbnail(Path((tess, id)): Path<(String, String)>, State(engine): Engine, Auth(principal): Auth, headers: HeaderMap) -> ApiResult {
    principal.require(Permission::Read, &tess)?;
    user_tessellation(&engine, &tess)?;
    let (bytes, content_type, chunk) = engine.file_thumbnail(&tess, &id).await?;
    let etag = format!("\"t-{}\"", &chunk[..32]);
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).is_some_and(|v| v.trim() == etag) {
        return Ok(StatusCode::NOT_MODIFIED.into_response());
    }
    let mut response = (StatusCode::OK, bytes.as_ref().clone()).into_response();
    let h = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=86400"));
    Ok(response)
}

/// A file's current version and its earlier ones.
pub async fn versions(Path((tess, id)): Path<(String, String)>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require(Permission::Read, &tess)?;
    user_tessellation(&engine, &tess)?;
    engine.files_tessellation(&tess)?;
    let doc = engine
        .get_document(&tess, &id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("File {} not found in '{}'.", id, tess)))?;
    let json = doc.to_api_json();
    let mut current = serde_json::Map::new();
    for key in ["version", "size", "content_type", "sha256", "modified_at", "uploaded_at", "uploaded_by"] {
        if let Some(v) = json.get(key) {
            current.insert(key.into(), v.clone());
        }
    }
    Ok(Json(json!({ "id": id, "current": current, "versions": json.get("versions").cloned().unwrap_or(json!([])) })).into_response())
}

/// Make an earlier version current again.
pub async fn restore_version(Path((tess, id, version)): Path<(String, String, u64)>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require(Permission::Restore, &tess)?;
    user_tessellation(&engine, &tess)?;
    let doc = engine.restore_file_version(&tess, &id, version).await?;
    Ok(respond(StatusCode::OK, doc.to_api_json(), false))
}

/// Deleted files from this tessellation.
pub async fn trash(Path(tess): Path<String>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require(Permission::Read, &tess)?;
    user_tessellation(&engine, &tess)?;
    let docs = engine.list_trash(&tess).await?;
    Ok(Json(json!({ "documents": docs_json(&docs), "total": docs.len() })).into_response())
}

/// Purge every file in the trash.
pub async fn empty_trash(Path(tess): Path<String>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require(Permission::Delete, &tess)?;
    user_tessellation(&engine, &tess)?;
    let purged = engine.empty_trash(&tess).await?;
    engine.audit(&principal.login, "files.empty_trash", &tess, json!({ "purged": purged })).await;
    Ok(Json(json!({ "purged": purged })).into_response())
}

/// Put a deleted file back.
pub async fn restore(
    Path((tess, id)): Path<(String, String)>,
    params: Result<Query<RestoreParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
) -> ApiResult {
    principal.require(Permission::Restore, &tess)?;
    let Query(params) = params?;
    user_tessellation(&engine, &tess)?;
    let doc = engine.restore_file(&tess, &id, params.name.as_deref(), params.path.as_deref()).await?;
    Ok(respond(StatusCode::OK, doc.to_api_json(), false))
}

/// Delete one file from the trash for good.
pub async fn purge(Path((tess, id)): Path<(String, String)>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require(Permission::Delete, &tess)?;
    user_tessellation(&engine, &tess)?;
    if !engine.purge_file(&tess, &id).await? {
        return Err(ApiError::not_found(format!("File {} is not in the trash of '{}'.", id, tess)));
    }
    engine.audit(&principal.login, "files.purge", &tess, json!({ "id": id })).await;
    Ok(no_content(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        let h = |s: &str| HeaderValue::from_str(s).unwrap();
        assert_eq!(parse_range(None, 100), Ok(None));
        assert_eq!(parse_range(Some(&h("bytes=0-9")), 100), Ok(Some((0, 10))));
        assert_eq!(parse_range(Some(&h("bytes=90-")), 100), Ok(Some((90, 100))));
        assert_eq!(parse_range(Some(&h("bytes=-5")), 100), Ok(Some((95, 100))));
        assert_eq!(parse_range(Some(&h("bytes=50-500")), 100), Ok(Some((50, 100))));
        assert_eq!(parse_range(Some(&h("bytes=0-1,5-6")), 100), Ok(None));
        assert_eq!(parse_range(Some(&h("bytes=100-")), 100), Err(()));
        assert_eq!(parse_range(Some(&h("bytes=-0")), 100), Err(()));
        assert_eq!(parse_range(Some(&h("bytes=abc")), 100), Err(()));
        assert_eq!(parse_range(Some(&h("items=0-1")), 100), Ok(None));
    }

    #[test]
    fn dispositions() {
        assert_eq!(disposition("inline", "a b.jpg"), "inline; filename=\"a b.jpg\"; filename*=UTF-8''a%20b.jpg");
        assert_eq!(disposition("attachment", "Résumé\".pdf"), "attachment; filename=\"R_sum__.pdf\"; filename*=UTF-8''R%C3%A9sum%C3%A9%22.pdf");
        assert!(shown_inline("image/jpeg") && shown_inline("audio/x-wav") && !shown_inline("image/svg+xml") && !shown_inline("text/html"));
    }
}
