// HexDB HTTP handlers.
//
// Documents are returned as plain JSON: `{"id": "...", ...fields}`, plus
// `_expires_at` when a TTL is set. Errors are returned as
// `{"error": {"code": "...", "message": "..."}}`.
//
// Writes accept an `Idempotency-Key` header. Repeating a request with the same
// key returns the original result (with `Idempotent-Replayed: true`) instead of
// writing again; reusing a key with a different request returns 422.

// Handlers take one argument per Axum extractor.
#![allow(clippy::too_many_arguments)]

use axum::{
    extract::{
        rejection::{JsonRejection, QueryRejection},
        Path, Query, State,
    },
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    Extension, Json,
};
use chrono::{DateTime, Utc};
use crate::auth::{Auth, MaybeAuth};
use hexdb_core::{
    constant_time_eq, Permission,
    engine::HexDBEngine,
    metrics::{collect, HexMeta},
    users::{self, NewUser, UserChanges},
    Document, DocumentQuery, EngineError, Filter, IdempotencyKey, Outcome, SortKey, TessellationInfo,
    SHUTDOWN_TOKEN_HEADER,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::watch;
use tracing::{error, info, warn};
use ulid::Ulid;

/// Header carrying the client's idempotency key.
pub const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
/// Response header set when a result is replayed from an idempotency record.
pub const IDEMPOTENT_REPLAYED_HEADER: &str = "idempotent-replayed";

const DEFAULT_PAGE_SIZE: usize = 100;
const MAX_PAGE_SIZE: usize = 1000;

pub(crate) type Engine = State<Arc<HexDBEngine>>;
pub(crate) type ApiResult = Result<Response, ApiError>;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// An API error rendered as `{"error": {"code", "message"}}`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    retry_after: Option<u64>,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError { status, code, message: message.into(), retry_after: None }
    }
    pub(crate) fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds);
        self
    }
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }
    pub(crate) fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    fn from_status(status: StatusCode, message: String) -> Self {
        let code = match status {
            StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
            StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
            _ => "invalid_request",
        };
        // Report malformed or mistyped JSON as a plain 400.
        let status = if status == StatusCode::UNPROCESSABLE_ENTITY { StatusCode::BAD_REQUEST } else { status };
        ApiError::new(status, code, message)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        match e.downcast_ref::<EngineError>() {
            Some(EngineError::NotFound(m)) => ApiError::not_found(m.clone()),
            Some(EngineError::Invalid(m)) => ApiError::invalid(m.clone()),
            Some(EngineError::Conflict(m)) => ApiError::conflict(m.clone()),
            Some(EngineError::Unprocessable(m)) => {
                ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "idempotency_key_reused", m.clone())
            }
            Some(EngineError::ReadOnly(m)) => ApiError::new(StatusCode::MISDIRECTED_REQUEST, "read_only_replica", m.clone()),
            Some(EngineError::Unauthorized(m)) => ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", m.clone()),
            Some(EngineError::Forbidden(m)) => ApiError::forbidden(m.clone()),
            Some(EngineError::RateLimited(m, retry)) => {
                ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", m.clone()).with_retry_after(*retry)
            }
            None => {
                error!("❌ Request failed: {:#}", e);
                ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "The request failed; see the server log.")
            }
        }
    }
}

impl From<JsonRejection> for ApiError {
    fn from(r: JsonRejection) -> Self {
        ApiError::from_status(r.status(), r.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(r: QueryRejection) -> Self {
        ApiError::from_status(r.status(), r.body_text())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(json!({ "error": { "code": self.code, "message": self.message } }))).into_response();
        if let Some(seconds) = self.retry_after {
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Optional query parameters for writes.
#[derive(Debug, Default, Deserialize)]
pub struct WriteParams {
    /// Time to live in seconds. The document expires this long after the write.
    pub ttl: Option<u64>,
}

impl WriteParams {
    /// The expiry time in epoch milliseconds, if a TTL was given.
    fn expiry(&self) -> Option<i64> {
        self.ttl.map(|secs| {
            let millis = secs.min(i64::MAX as u64 / 1000) as i64 * 1000;
            Utc::now().timestamp_millis().saturating_add(millis)
        })
    }
}

/// Query-string options for listing documents:
/// `?filter=<JSON>&sort=-views,title&limit=100&offset=0&after=<id>`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    /// A filter as URL-encoded JSON (see the README's "Filters").
    pub filter: Option<String>,
    /// Comma-separated field paths; prefix with `-` for descending.
    pub sort: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub after: Option<String>,
    /// Comma-separated field paths to return (default: all fields).
    pub fields: Option<String>,
}

/// JSON body for `POST /{tessellation}/_query`, for filters too long for a URL.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryRequest {
    #[serde(default)]
    pub filter: Value,
    pub sort: Option<SortSpec>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub after: Option<String>,
    /// Field paths to return: `["name", "address.city"]` or `"name,address.city"`.
    pub fields: Option<FieldList>,
}

/// Field paths as a list or a comma-separated string.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum FieldList {
    Text(String),
    List(Vec<String>),
}

impl FieldList {
    fn paths(self) -> Vec<String> {
        match self {
            FieldList::Text(t) => split_fields(&t),
            FieldList::List(l) => l.into_iter().filter(|f| !f.trim().is_empty()).collect(),
        }
    }
}

fn split_fields(text: &str) -> Vec<String> {
    text.split(',').map(str::trim).filter(|f| !f.is_empty()).map(String::from).collect()
}

/// Sort keys as `"-views,title"` or `[{"field": "views", "descending": true}]`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SortSpec {
    Text(String),
    Keys(Vec<SortKey>),
}

/// Query-string options for counting documents: `?filter=<JSON>`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CountParams {
    pub filter: Option<String>,
}

fn parse_filter_param(filter: Option<&str>) -> Result<Value, ApiError> {
    match filter.map(str::trim) {
        None | Some("") => Ok(Value::Null),
        Some(text) => serde_json::from_str(text).map_err(|e| ApiError::invalid(format!("filter is not valid JSON: {}", e))),
    }
}

/// Parse `"-views,title"` into sort keys.
fn parse_sort_text(text: &str) -> Result<Vec<SortKey>, ApiError> {
    text.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (descending, field) = match part.strip_prefix('-') {
                Some(field) => (true, field),
                None => (false, part.strip_prefix('+').unwrap_or(part)),
            };
            if field.is_empty() {
                return Err(ApiError::invalid(format!("sort: invalid key '{}'.", part)));
            }
            Ok(SortKey { field: field.to_string(), descending })
        })
        .collect()
}

/// Run a document query and render `{"documents", "total", "next"}`.
#[allow(clippy::too_many_arguments)]
async fn run_query(
    engine: &HexDBEngine,
    tess: &str,
    filter: &Value,
    sort: Vec<SortKey>,
    limit: Option<usize>,
    offset: Option<usize>,
    after: Option<&str>,
    fields: Option<Vec<String>>,
) -> ApiResult {
    existing_tessellation(engine, tess)?;
    let limit = limit.unwrap_or(DEFAULT_PAGE_SIZE);
    if limit > MAX_PAGE_SIZE {
        return Err(ApiError::invalid(format!("limit must be 0-{}.", MAX_PAGE_SIZE)));
    }
    let after = match after {
        Some(a) => Some(Ulid::from_string(a).map_err(|_| ApiError::invalid("after must be a document ID."))?),
        None => None,
    };
    let query = DocumentQuery {
        filter: Filter::parse(filter)?,
        sort,
        offset: offset.unwrap_or(0),
        limit,
        after,
    };

    let page = engine.query_documents(tess, &query).await?;
    let documents = match &fields {
        Some(f) if !f.is_empty() => Value::Array(page.documents.iter().map(|d| hexdb_core::project(&d.to_api_json(), f)).collect()),
        _ => docs_json(&page.documents),
    };
    Ok(Json(json!({
        "documents": documents,
        "total": page.total,
        "next": page.next.map(|id| id.to_string()),
        "plan": { "indexes": page.indexes, "scanned": page.scanned },
    }))
    .into_response())
}

/// Build an idempotency key from the request, if the client sent one. The
/// fingerprint covers the method, path, query and canonical JSON body.
fn idempotency(
    principal: &hexdb_core::Principal,
    headers: &HeaderMap,
    method: &Method,
    uri: &Uri,
    body: Option<&Value>,
) -> Result<Option<IdempotencyKey>, ApiError> {
    let Some(value) = headers.get(IDEMPOTENCY_KEY_HEADER) else { return Ok(None) };
    let key = value
        .to_str()
        .map_err(|_| ApiError::invalid("Idempotency-Key must be printable ASCII."))?;

    let mut request = format!("{} {}\n", method, uri.path_and_query().map(|p| p.as_str()).unwrap_or(uri.path())).into_bytes();
    if let Some(body) = body {
        // serde_json orders object keys, so this is canonical.
        request.extend(serde_json::to_vec(body).map_err(|e| ApiError::invalid(e.to_string()))?);
    }
    Ok(Some(IdempotencyKey::new(key, &request)?.scoped_to(&principal.user_id)))
}

/// A JSON response, marked as replayed if it came from an idempotency record.
fn respond(status: StatusCode, body: Value, replayed: bool) -> Response {
    let mut response = (status, Json(body)).into_response();
    if replayed {
        response
            .headers_mut()
            .insert(IDEMPOTENT_REPLAYED_HEADER, HeaderValue::from_static("true"));
    }
    response
}

fn with_location(mut response: Response, location: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
}

fn no_content(replayed: bool) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    if replayed {
        response
            .headers_mut()
            .insert(IDEMPOTENT_REPLAYED_HEADER, HeaderValue::from_static("true"));
    }
    response
}

fn millis_to_rfc3339(millis: i64) -> Value {
    DateTime::from_timestamp_millis(millis)
        .map(|t| Value::String(t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)))
        .unwrap_or(Value::Null)
}

/// Reject names that aren't valid user tessellations. System tessellations
/// (users, roles, idempotency records) have their own APIs.
fn user_tessellation(engine: &HexDBEngine, tess: &str) -> Result<(), ApiError> {
    hexdb_core::validate_tessellation_name(tess).map_err(|e| ApiError::invalid(e.to_string()))?;
    if engine.is_system_tessellation(tess) {
        return Err(ApiError::forbidden(format!(
            "'{}' is a system tessellation; use its dedicated API (e.g. /users, /roles).",
            tess
        )));
    }
    Ok(())
}

fn existing_tessellation(engine: &HexDBEngine, tess: &str) -> Result<(), ApiError> {
    user_tessellation(engine, tess)?;
    if !engine.tessellation_exists(tess) {
        return Err(ApiError::not_found(format!("Tessellation '{}' not found.", tess)));
    }
    Ok(())
}

fn docs_json(docs: &[Document]) -> Value {
    Value::Array(docs.iter().map(Document::to_api_json).collect())
}

/// Accept a bare array or `{"documents": [...]}`.
fn bulk_items(body: Value) -> Result<Vec<Value>, ApiError> {
    match body {
        Value::Array(items) => Ok(items),
        Value::Object(mut map) => match map.remove("documents") {
            Some(Value::Array(items)) => Ok(items),
            _ => Err(ApiError::invalid("Expected a JSON array of documents, or {\"documents\": [...]}.")),
        },
        _ => Err(ApiError::invalid("Expected a JSON array of documents, or {\"documents\": [...]}.")),
    }
}

/// Split bulk update items into (id, fields); each item must have an "id".
fn bulk_targets(items: Vec<Value>) -> Result<Vec<(String, Value)>, ApiError> {
    items
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::invalid(format!("documents[{}]: missing \"id\".", i)))?
                .to_string();
            Ok((id, item))
        })
        .collect()
}

fn ids_json(docs: &[Document]) -> Value {
    json!({ "count": docs.len(), "ids": docs.iter().map(|d| d.id.to_string()).collect::<Vec<_>>() })
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/// Lets the shutdown endpoint trigger the server's graceful shutdown.
/// The token is generated at startup and written to the runtime file in the data directory.
#[derive(Clone)]
pub struct ShutdownHandle {
    pub token: Arc<String>,
    pub trigger: Arc<watch::Sender<()>>,
}

/// Liveness check. Does not lock the storage engine.
/// Without credentials it only says the server is up; signed-in users also
/// see the hex's identity and version.
pub async fn health(State(engine): Engine, MaybeAuth(principal): MaybeAuth) -> Json<Value> {
    if principal.is_none() {
        return Json(json!({ "status": "ok" }));
    }
    Json(json!({
        "status": "ok",
        "id": engine.id.to_string(),
        "name": engine.name,
        "hex_type": engine.role(),
        "version": engine.version,
        "uptime_seconds": (Utc::now() - engine.start_datetime).num_seconds().max(0),
    }))
}

/// Begin a graceful shutdown. Requires the token from the runtime file in the `x-hexdb-shutdown-token` header.
/// An administrator's session or API key also works.
pub async fn shutdown(Extension(handle): Extension<ShutdownHandle>, MaybeAuth(principal): MaybeAuth, headers: HeaderMap) -> StatusCode {
    let presented = headers
        .get(SHUTDOWN_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token_ok = !presented.is_empty() && constant_time_eq(presented.as_bytes(), handle.token.as_bytes());
    let admin = principal.as_ref().is_some_and(|p| p.is_admin());

    if !token_ok && !admin {
        warn!("⚠️ Rejected shutdown request without a valid token or admin credentials.");
        return if principal.is_some() { StatusCode::FORBIDDEN } else { StatusCode::UNAUTHORIZED };
    }

    match &principal {
        Some(p) if !token_ok => info!("🛑 Shutdown requested through the API by '{}'...", p.login),
        _ => info!("🛑 Shutdown requested through the API..."),
    }
    let _ = handle.trigger.send(());
    StatusCode::ACCEPTED
}

pub async fn status(State(engine): Engine, Auth(principal): Auth) -> Result<Json<HexMeta>, ApiError> {
    principal.require_admin()?;
    Ok(Json(collect(&engine).await))
}

/// Flush unflushed writes to SSTables.
pub async fn flush(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    let stats = engine.flush().await?;
    Ok(Json(json!({
        "entries": stats.entries,
        "tessellations": stats.tessellations,
        "wal_segments_deleted": stats.wal_segments_deleted,
    }))
    .into_response())
}

/// Compact SSTables now. Also re-encrypts files written with a previous key.
pub async fn compact(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    let stats = engine.compact().await?;
    Ok(Json(json!({
        "tessellations": stats.tessellations,
        "files_merged": stats.files_merged,
        "entries_kept": stats.entries_kept,
        "entries_dropped": stats.entries_dropped,
        "sstable_files_on_old_keys": engine.sst_files_needing_rewrite().await,
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// Tessellations
// ---------------------------------------------------------------------------

fn tessellation_json(name: &str, info: &TessellationInfo) -> Value {
    json!({
        "name": name,
        "kind": info.kind,
        "created": millis_to_rfc3339(info.created),
        "indexes": info.indexes.iter().map(|i| i.name.clone()).collect::<Vec<_>>(),
    })
}

/// A tessellation's indexes with their statistics.
pub async fn list_indexes(Path(name): Path<String>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require(Permission::Read, &name)?;
    existing_tessellation(&engine, &name)?;
    Ok(Json(json!({ "indexes": engine.list_indexes(&name) })).into_response())
}

/// Create an index: `{"fields": ["status"], "name"?, "kind"?: "field" | "text", "unique"?: bool}`.
/// Builds it from the existing documents before returning 201.
pub async fn create_index(
    Path(name): Path<String>,
    State(engine): Engine,
    Auth(principal): Auth,
    body: Result<Json<hexdb_core::IndexDef>, JsonRejection>,
) -> ApiResult {
    principal.require(Permission::Manage, &name)?;
    let Json(def) = body?;
    existing_tessellation(&engine, &name)?;
    let info = engine.create_index(&name, def).await?;
    Ok((StatusCode::CREATED, Json(json!(info))).into_response())
}

/// Drop an index.
pub async fn drop_index(Path((name, index)): Path<(String, String)>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require(Permission::Manage, &name)?;
    existing_tessellation(&engine, &name)?;
    if !engine.drop_index(&name, &index)? {
        return Err(ApiError::not_found(format!("'{}' has no index named '{}'.", name, index)));
    }
    Ok(no_content(false))
}

/// List tessellations.
/// Tessellations the caller can read (administrators see all, including system ones).
pub async fn list_tessellations(State(engine): Engine, Auth(principal): Auth) -> Json<Value> {
    let list: Vec<Value> = engine
        .tessellation_details()
        .iter()
        .filter(|(name, info)| {
            if principal.is_admin() {
                return !name.starts_with('_');
            }
            info.kind == "user" && principal.can(Permission::Read, name)
        })
        .map(|(name, info)| tessellation_json(name, info))
        .collect();
    Json(json!({ "tessellations": list }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewTessellation {
    pub name: String,
    pub kind: Option<String>,
}

/// Create a tessellation: `{"name": "...", "kind": "user"}`.
pub async fn create_tessellation(
    State(engine): Engine,
    Auth(principal): Auth,
    body: Result<Json<NewTessellation>, JsonRejection>,
) -> ApiResult {
    let Json(input) = body?;
    principal.require(Permission::Write, &input.name)?;
    if input.kind.as_deref().is_some_and(|k| k != "user") {
        return Err(ApiError::invalid("Only \"user\" tessellations can be created through the API."));
    }
    user_tessellation(&engine, &input.name)?;
    if !engine.create_tessellation(&input.name, "user")? {
        return Err(ApiError::conflict(format!("Tessellation '{}' already exists.", input.name)));
    }
    let info = engine.tessellation_info(&input.name).ok_or_else(|| ApiError::not_found("Tessellation vanished."))?;
    let response = respond(StatusCode::CREATED, tessellation_json(&input.name, &info), false);
    Ok(with_location(response, &format!("/tessellations/{}", input.name)))
}

/// One tessellation, with its document count.
pub async fn get_tessellation(Path(name): Path<String>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    if engine.is_system_tessellation(&name) {
        principal.require_admin()?;
    } else {
        principal.require(Permission::Read, &name)?;
    }
    let info = engine
        .tessellation_info(&name)
        .ok_or_else(|| ApiError::not_found(format!("Tessellation '{}' not found.", name)))?;
    let mut body = tessellation_json(&name, &info);
    body["document_count"] = json!(engine.count_documents(&name).await?);
    Ok(Json(body).into_response())
}

/// Delete a tessellation and all of its documents.
pub async fn delete_tessellation(Path(name): Path<String>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require(Permission::Manage, &name)?;
    if engine.tessellation_exists(&name) && engine.is_system_tessellation(&name) {
        return Err(ApiError::forbidden(format!("'{}' is a system tessellation and can't be deleted.", name)));
    }
    if !engine.delete_tessellation(&name).await? {
        return Err(ApiError::not_found(format!("Tessellation '{}' not found.", name)));
    }
    Ok(no_content(false))
}

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

/// List documents, with optional filter, sort and paging:
/// `?filter=<JSON>&sort=-views,title&limit=100&offset=0&after=<id>`.
/// Without `sort`, results are in ID order and `next` pages forward via `after`.
pub async fn list_docs(
    Path(tess): Path<String>,
    params: Result<Query<ListParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
) -> ApiResult {
    principal.require(Permission::Read, &tess)?;
    let Query(params) = params?;
    let filter = parse_filter_param(params.filter.as_deref())?;
    let sort = parse_sort_text(params.sort.as_deref().unwrap_or(""))?;
    let fields = params.fields.as_deref().map(split_fields);
    run_query(&engine, &tess, &filter, sort, params.limit, params.offset, params.after.as_deref(), fields).await
}

/// Query documents with a JSON body:
/// `{"filter": {...}, "sort": "-views" | [{"field", "descending"}], "limit", "offset", "after"}`.
pub async fn query_docs(
    Path(tess): Path<String>,
    State(engine): Engine,
    Auth(principal): Auth,
    body: Result<Json<QueryRequest>, JsonRejection>,
) -> ApiResult {
    principal.require(Permission::Read, &tess)?;
    let Json(request) = body?;
    let sort = match request.sort {
        Some(SortSpec::Text(text)) => parse_sort_text(&text)?,
        Some(SortSpec::Keys(keys)) => keys,
        None => Vec::new(),
    };
    let fields = request.fields.map(FieldList::paths);
    run_query(&engine, &tess, &request.filter, sort, request.limit, request.offset, request.after.as_deref(), fields).await
}

/// Count documents, optionally only those matching `?filter=<JSON>`.
pub async fn count_docs(
    Path(tess): Path<String>,
    params: Result<Query<CountParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
) -> ApiResult {
    principal.require(Permission::Read, &tess)?;
    let Query(params) = params?;
    existing_tessellation(&engine, &tess)?;
    let filter = Filter::parse(&parse_filter_param(params.filter.as_deref())?)?;
    Ok(Json(json!({ "count": engine.count_matching(&tess, &filter).await? })).into_response())
}

/// Get a document.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetParams {
    /// Comma-separated field paths to return.
    pub fields: Option<String>,
}

pub async fn get_doc(
    Path((tess, id)): Path<(String, String)>,
    params: Result<Query<GetParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
) -> ApiResult {
    principal.require(Permission::Read, &tess)?;
    let Query(params) = params?;
    user_tessellation(&engine, &tess)?;
    match engine.get_document_versioned(&tess, &id).await? {
        Some((doc, version)) => {
            let json = match params.fields.as_deref().map(split_fields) {
                Some(f) if !f.is_empty() => hexdb_core::project(&doc.to_api_json(), &f),
                _ => doc.to_api_json(),
            };
            let mut response = Json(json).into_response();
            if let Ok(etag) = HeaderValue::from_str(&format!("\"{}\"", version)) {
                response.headers_mut().insert(header::ETAG, etag);
            }
            Ok(response)
        }
        None => Err(ApiError::not_found(format!("Document {} not found in '{}'.", id, tess))),
    }
}

/// Insert a document. Returns 201 with the stored document.
pub async fn insert_doc(
    Path(tess): Path<String>,
    params: Result<Query<WriteParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    principal.require(Permission::Write, &tess)?;
    let (Query(params), Json(body)) = (params?, body?);
    user_tessellation(&engine, &tess)?;
    let idem = idempotency(&principal, &headers, &method, &uri, Some(&body))?;

    let outcome = engine.insert_documents(&tess, vec![body], params.expiry(), idem).await?;
    let doc = &outcome.value[0];
    let response = respond(StatusCode::CREATED, doc.to_api_json(), outcome.replayed);
    Ok(with_location(response, &format!("/{}/{}", tess, doc.id)))
}

/// Replace a document's fields.
pub async fn replace_doc(
    Path((tess, id)): Path<(String, String)>,
    params: Result<Query<WriteParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    principal.require(Permission::Write, &tess)?;
    let (Query(params), Json(body)) = (params?, body?);
    user_tessellation(&engine, &tess)?;
    let idem = idempotency(&principal, &headers, &method, &uri, Some(&body))?;
    let outcome = engine.replace_document(&tess, &id, body, params.expiry(), idem).await?;
    Ok(respond(StatusCode::OK, outcome.value.to_api_json(), outcome.replayed))
}

/// Merge fields into a document (a `null` field removes it).
pub async fn patch_doc(
    Path((tess, id)): Path<(String, String)>,
    params: Result<Query<WriteParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    principal.require(Permission::Write, &tess)?;
    let (Query(params), Json(body)) = (params?, body?);
    user_tessellation(&engine, &tess)?;
    let idem = idempotency(&principal, &headers, &method, &uri, Some(&body))?;
    let outcome = engine.patch_document(&tess, &id, body, params.expiry(), idem).await?;
    Ok(respond(StatusCode::OK, outcome.value.to_api_json(), outcome.replayed))
}

/// Delete a document. Returns 204, or 404 if it doesn't exist.
pub async fn delete_doc(
    Path((tess, id)): Path<(String, String)>,
    State(engine): Engine,
    Auth(principal): Auth,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult {
    principal.require(Permission::Write, &tess)?;
    user_tessellation(&engine, &tess)?;
    let idem = idempotency(&principal, &headers, &method, &uri, None)?;
    let outcome = engine.delete_document(&tess, &id, idem).await?;
    if !outcome.value {
        return Err(ApiError::not_found(format!("Document {} not found in '{}'.", id, tess)));
    }
    Ok(no_content(outcome.replayed))
}

// ---------------------------------------------------------------------------
// Bulk
// ---------------------------------------------------------------------------

/// Insert many documents atomically. Body: an array, or `{"documents": [...]}`.
pub async fn bulk_insert(
    Path(tess): Path<String>,
    params: Result<Query<WriteParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    principal.require(Permission::Write, &tess)?;
    let (Query(params), Json(body)) = (params?, body?);
    user_tessellation(&engine, &tess)?;
    // The response lists IDs only, so the stored result does too.
    let idem = idempotency(&principal, &headers, &method, &uri, Some(&body))?.map(IdempotencyKey::compact);
    let outcome = engine.insert_documents(&tess, bulk_items(body)?, params.expiry(), idem).await?;
    Ok(respond(StatusCode::CREATED, ids_json(&outcome.value), outcome.replayed))
}

/// Replace many documents atomically. Each item must include its `id`.
pub async fn bulk_replace(
    Path(tess): Path<String>,
    params: Result<Query<WriteParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    principal.require(Permission::Write, &tess)?;
    let (Query(params), Json(body)) = (params?, body?);
    user_tessellation(&engine, &tess)?;
    // The response lists IDs only, so the stored result does too.
    let idem = idempotency(&principal, &headers, &method, &uri, Some(&body))?.map(IdempotencyKey::compact);
    let targets = bulk_targets(bulk_items(body)?)?;
    let outcome = engine.replace_documents(&tess, targets, params.expiry(), idem).await?;
    Ok(respond(StatusCode::OK, ids_json(&outcome.value), outcome.replayed))
}

/// Merge-patch many documents atomically. Each item must include its `id`.
pub async fn bulk_patch(
    Path(tess): Path<String>,
    params: Result<Query<WriteParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    principal.require(Permission::Write, &tess)?;
    let (Query(params), Json(body)) = (params?, body?);
    user_tessellation(&engine, &tess)?;
    // The response lists IDs only, so the stored result does too.
    let idem = idempotency(&principal, &headers, &method, &uri, Some(&body))?.map(IdempotencyKey::compact);
    let targets = bulk_targets(bulk_items(body)?)?;
    let outcome = engine.patch_documents(&tess, targets, params.expiry(), idem).await?;
    Ok(respond(StatusCode::OK, ids_json(&outcome.value), outcome.replayed))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRequest {
    pub filter: Value,
    pub update: Value,
}

/// Merge-patch every document matching a filter, atomically:
/// `{"filter": {"status": "draft"}, "update": {"status": "published"}}`.
pub async fn update_where(
    Path(tess): Path<String>,
    params: Result<Query<WriteParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    principal.require(Permission::Write, &tess)?;
    let (Query(params), Json(body)) = (params?, body?);
    existing_tessellation(&engine, &tess)?;
    let idem = idempotency(&principal, &headers, &method, &uri, Some(&body))?;
    let request: UpdateRequest = serde_json::from_value(body).map_err(|e| ApiError::invalid(e.to_string()))?;
    let outcome = engine
        .update_where(&tess, &request.filter, &request.update, params.expiry(), idem)
        .await?;
    Ok(respond(StatusCode::OK, serde_json::to_value(&outcome.value).unwrap_or_default(), outcome.replayed))
}

// ---------------------------------------------------------------------------
// Users and roles
// ---------------------------------------------------------------------------

fn user_response(outcome: Outcome<users::UserView>, status: StatusCode) -> Response {
    respond(status, serde_json::to_value(&outcome.value).unwrap_or_default(), outcome.replayed)
}

/// List users.
pub async fn list_users(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    Ok(Json(json!({ "users": users::list_users(&engine).await? })).into_response())
}

/// Create a user: `{"login", "password", "email_address", "roles": [{"name", "permissions": [...]}]}`.
pub async fn create_user(
    State(engine): Engine,
    Auth(principal): Auth,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    principal.require_admin()?;
    let Json(body) = body?;
    let idem = idempotency(&principal, &headers, &method, &uri, Some(&body))?;
    let input: NewUser = serde_json::from_value(body).map_err(|e| ApiError::invalid(e.to_string()))?;
    let outcome = users::create_user(&engine, input, idem).await?;
    let location = format!("/users/{}", outcome.value.id);
    Ok(with_location(user_response(outcome, StatusCode::CREATED), &location))
}

/// Get a user by ID or login.
pub async fn get_user(Path(user): Path<String>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    match users::get_user(&engine, &user).await? {
        Some(view) => Ok(Json(view).into_response()),
        None => Err(ApiError::not_found(format!("User '{}' not found.", user))),
    }
}

#[allow(clippy::too_many_arguments)]
async fn change_user(
    principal: &hexdb_core::Principal,
    engine: &HexDBEngine,
    user: &str,
    full_replace: bool,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    principal.require_admin()?;
    let Json(body) = body?;
    let idem = idempotency(principal, headers, method, uri, Some(&body))?;
    let changes: UserChanges = serde_json::from_value(body).map_err(|e| ApiError::invalid(e.to_string()))?;
    let outcome = users::update_user(engine, user, changes, full_replace, idem).await?;
    Ok(user_response(outcome, StatusCode::OK))
}

/// Replace a user's editable fields (`email_address` and `roles` required).
pub async fn replace_user(
    Path(user): Path<String>,
    State(engine): Engine,
    Auth(principal): Auth,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    change_user(&principal, &engine, &user, true, &method, &uri, &headers, body).await
}

/// Change some of a user's fields.
pub async fn patch_user(
    Path(user): Path<String>,
    State(engine): Engine,
    Auth(principal): Auth,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    change_user(&principal, &engine, &user, false, &method, &uri, &headers, body).await
}

/// Delete a user.
pub async fn delete_user(
    Path(user): Path<String>,
    State(engine): Engine,
    Auth(principal): Auth,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult {
    principal.require_admin()?;
    let idem = idempotency(&principal, &headers, &method, &uri, None)?;
    let outcome = users::delete_user(&engine, &user, idem).await?;
    if !outcome.value {
        return Err(ApiError::not_found(format!("User '{}' not found.", user)));
    }
    Ok(no_content(outcome.replayed))
}

/// List roles.
pub async fn list_roles(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    Ok(Json(json!({ "roles": users::list_roles(&engine).await? })).into_response())
}

/// Get a role by name.
pub async fn get_role(Path(name): Path<String>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    match users::get_role(&engine, &name).await? {
        Some(role) => Ok(Json(role).into_response()),
        None => Err(ApiError::not_found(format!("Role '{}' not found.", name))),
    }
}

// ---------------------------------------------------------------------------
// GraphQL
// ---------------------------------------------------------------------------

/// Execute a GraphQL request: `{"query": "...", "variables": {...}, "operationName": "..."}`.
pub async fn graphql(
    Extension(schema): Extension<hexdb_query::HexDBSchema>,
    Auth(principal): Auth,
    body: Result<Json<async_graphql::Request>, JsonRejection>,
) -> ApiResult {
    let Json(request) = body?;
    let (response, replays) = hexdb_query::execute(&schema, request.data(principal)).await;
    // Replayed mutations are also listed in `extensions.idempotentReplays`.
    Ok(respond_graphql(response, !replays.is_empty()))
}

fn respond_graphql(response: async_graphql::Response, replayed: bool) -> Response {
    let mut http = Json(response).into_response();
    if replayed {
        http.headers_mut()
            .insert(IDEMPOTENT_REPLAYED_HEADER, HeaderValue::from_static("true"));
    }
    http
}

// ---------------------------------------------------------------------------
// Metrics history
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryParams {
    /// How far back to return samples, 1-360 minutes (default 60).
    pub minutes: Option<i64>,
}

/// Recent metrics samples: `{"interval_seconds", "samples": [...]}`. Kept in memory for 6 hours.
pub async fn status_history(
    params: Result<Query<HistoryParams>, QueryRejection>,
    State(engine): Engine,
    Auth(principal): Auth,
) -> ApiResult {
    principal.require_admin()?;
    let Query(params) = params?;
    let minutes = params.minutes.unwrap_or(60).clamp(1, 360);
    let since = Utc::now() - chrono::Duration::minutes(minutes);
    Ok(Json(json!({
        "interval_seconds": hexdb_core::metrics::HISTORY_INTERVAL_SECONDS,
        "samples": engine.history.since(since),
    }))
    .into_response())
}

#[derive(Deserialize)]
pub struct LogParams {
    /// Minimum level: error, warn, info, debug, or trace.
    pub level: Option<String>,
    /// Only records after this sequence number (for tailing).
    pub after: Option<u64>,
    /// Only records before this sequence number (for paging back).
    pub before: Option<u64>,
    /// Case-insensitive text search.
    pub q: Option<String>,
    /// Target module prefix, e.g. `hexdb_core::network`.
    pub target: Option<String>,
    pub limit: Option<usize>,
}

/// Loaded plugins and their delivery state: `{"plugins": [...], "registry"}`.
pub async fn plugins(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    Ok(Json(json!({
        "plugins": engine.plugins.list(),
        "registry": hexdb_core::plugins::registry_path(&engine).display().to_string(),
        "enabled": engine.config.plugins.enabled,
    }))
    .into_response())
}

/// Recent server log records: `{"records": [...], "last_seq", "capacity"}`.
/// Records are oldest first. Poll with `after=<last seq seen>` to tail.
pub async fn logs(params: Result<Query<LogParams>, QueryRejection>, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    let Query(params) = params?;
    let level = match params.level.as_deref().filter(|l| !l.is_empty()) {
        Some(l) => Some(
            hexdb_core::parse_level(l)
                .ok_or_else(|| ApiError::invalid(format!("Unknown level '{}'. Use error, warn, info, debug, or trace.", l)))?,
        ),
        None => None,
    };
    let buffer = hexdb_core::log_buffer();
    let records = buffer.query(&hexdb_core::LogQuery {
        level,
        after: params.after,
        before: params.before,
        search: params.q,
        target: params.target.filter(|t| !t.is_empty()),
        limit: params.limit.unwrap_or(200).clamp(1, 1000),
    });
    Ok(Json(json!({
        "records": records,
        "last_seq": buffer.last_seq(),
        "capacity": buffer.capacity(),
    }))
    .into_response())
}

/// Group and summarize matching documents:
/// `{"filter", "group_by": [...], "aggregates": {"name": {"$sum": "field"}}, "sort", "offset", "limit"}`.
/// Returns `{"rows": [...], "total_groups", "matched"}`.
pub async fn aggregate_docs(
    Path(tess): Path<String>,
    State(engine): Engine,
    Auth(principal): Auth,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    principal.require(Permission::Read, &tess)?;
    let Json(mut body) = body?;
    existing_tessellation(&engine, &tess)?;
    // Accept the same "field desc, other" sort text as the query endpoints.
    if let Some(Value::String(text)) = body.get("sort") {
        let keys = parse_sort_text(text)?;
        body["sort"] = serde_json::to_value(keys).unwrap_or(Value::Null);
    }
    let aggregation = hexdb_core::Aggregation::from_json(&body)?;
    Ok(Json(engine.aggregate(&tess, &aggregation).await?).into_response())
}

/// Run operations across tessellations atomically:
/// `{"operations": [{"op": "insert|replace|patch|delete|get|check", "tessellation", "id", "data", "ttl", "if_version", "if_match"}]}`.
/// Every operation succeeds or nothing is written. Returns `{"results": [...], "writes"}`;
/// a failed precondition is a 409 and a missing document a 404.
pub async fn transaction(
    State(engine): Engine,
    Auth(principal): Auth,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    let Json(body) = body?;
    let ops = hexdb_core::parse_transaction(&body)?;
    for op in &ops {
        let needed = match op.kind {
            hexdb_core::TxOpKind::Get | hexdb_core::TxOpKind::Check => Permission::Read,
            _ => Permission::Write,
        };
        principal.require(needed, &op.tessellation)?;
    }
    let idem = idempotency(&principal, &headers, &method, &uri, Some(&body))?;
    let outcome = engine.transaction(&ops, idem).await?;
    Ok(respond(StatusCode::OK, serde_json::to_value(&outcome.value).unwrap_or(Value::Null), outcome.replayed))
}
