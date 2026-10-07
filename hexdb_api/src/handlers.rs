// HexDB HTTP handlers.
//
// Documents are returned as plain JSON: `{"id": "...", ...fields}`, plus
// `_expires_at` when a TTL is set. Errors are returned as
// `{"error": {"code": "...", "message": "..."}}`.
//
// Writes accept an `Idempotency-Key` header. Repeating a request with the same
// key returns the original result (with `Idempotent-Replayed: true`) instead of
// writing again; reusing a key with a different request returns 422.

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
use hexdb_core::{
    constant_time_eq,
    engine::HexDBEngine,
    metrics::{collect, HexMeta},
    users::{self, NewUser, UserChanges},
    Document, EngineError, IdempotencyKey, Outcome, TessellationInfo, SHUTDOWN_TOKEN_HEADER,
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

type Engine = State<Arc<HexDBEngine>>;
type ApiResult = Result<Response, ApiError>;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// An API error rendered as `{"error": {"code", "message"}}`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError { status, code, message: message.into() }
    }
    fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }
    fn forbidden(message: impl Into<String>) -> Self {
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
        (self.status, Json(json!({ "error": { "code": self.code, "message": self.message } }))).into_response()
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

/// Query parameters for listing documents.
#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    pub limit: Option<usize>,
    pub after: Option<String>,
}

/// Build an idempotency key from the request, if the client sent one. The
/// fingerprint covers the method, path, query and canonical JSON body.
fn idempotency(headers: &HeaderMap, method: &Method, uri: &Uri, body: Option<&Value>) -> Result<Option<IdempotencyKey>, ApiError> {
    let Some(value) = headers.get(IDEMPOTENCY_KEY_HEADER) else { return Ok(None) };
    let key = value
        .to_str()
        .map_err(|_| ApiError::invalid("Idempotency-Key must be printable ASCII."))?;

    let mut request = format!("{} {}\n", method, uri.path_and_query().map(|p| p.as_str()).unwrap_or(uri.path())).into_bytes();
    if let Some(body) = body {
        // serde_json orders object keys, so this is canonical.
        request.extend(serde_json::to_vec(body).map_err(|e| ApiError::invalid(e.to_string()))?);
    }
    Ok(Some(IdempotencyKey::new(key, &request)?))
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
pub async fn health(State(engine): Engine) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "id": engine.id.to_string(),
        "name": engine.name,
        "hex_type": engine.hex_type,
        "version": engine.version,
        "uptime_seconds": (Utc::now() - engine.start_datetime).num_seconds().max(0),
    }))
}

/// Begin a graceful shutdown. Requires the token from the runtime file in the `x-hexdb-shutdown-token` header.
pub async fn shutdown(Extension(handle): Extension<ShutdownHandle>, headers: HeaderMap) -> StatusCode {
    let presented = headers
        .get(SHUTDOWN_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if presented.is_empty() || !constant_time_eq(presented.as_bytes(), handle.token.as_bytes()) {
        warn!("⚠️ Rejected shutdown request with a missing or invalid token.");
        return StatusCode::UNAUTHORIZED;
    }

    info!("🛑 Shutdown requested through the API...");
    let _ = handle.trigger.send(());
    StatusCode::ACCEPTED
}

pub async fn status(State(engine): Engine) -> Json<HexMeta> {
    Json(collect(&engine).await)
}

/// Flush unflushed writes to SSTables.
pub async fn flush(State(engine): Engine) -> ApiResult {
    let stats = engine.flush().await?;
    Ok(Json(json!({
        "entries": stats.entries,
        "tessellations": stats.tessellations,
        "wal_segments_deleted": stats.wal_segments_deleted,
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// Tessellations
// ---------------------------------------------------------------------------

fn tessellation_json(name: &str, info: &TessellationInfo) -> Value {
    json!({ "name": name, "kind": info.kind, "created": millis_to_rfc3339(info.created) })
}

/// List tessellations.
pub async fn list_tessellations(State(engine): Engine) -> Json<Value> {
    let list: Vec<Value> = engine
        .tessellation_details()
        .iter()
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
pub async fn create_tessellation(State(engine): Engine, body: Result<Json<NewTessellation>, JsonRejection>) -> ApiResult {
    let Json(input) = body?;
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
pub async fn get_tessellation(Path(name): Path<String>, State(engine): Engine) -> ApiResult {
    let info = engine
        .tessellation_info(&name)
        .ok_or_else(|| ApiError::not_found(format!("Tessellation '{}' not found.", name)))?;
    let mut body = tessellation_json(&name, &info);
    body["document_count"] = json!(engine.count_documents(&name).await?);
    Ok(Json(body).into_response())
}

/// Delete a tessellation and all of its documents.
pub async fn delete_tessellation(Path(name): Path<String>, State(engine): Engine) -> ApiResult {
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

/// List documents in ID order: `?limit=100&after=<id>`.
pub async fn list_docs(
    Path(tess): Path<String>,
    params: Result<Query<ListParams>, QueryRejection>,
    State(engine): Engine,
) -> ApiResult {
    let Query(params) = params?;
    existing_tessellation(&engine, &tess)?;
    let limit = params.limit.unwrap_or(DEFAULT_PAGE_SIZE);
    if limit == 0 || limit > MAX_PAGE_SIZE {
        return Err(ApiError::invalid(format!("limit must be 1-{}.", MAX_PAGE_SIZE)));
    }
    let after = match params.after.as_deref() {
        Some(a) => Some(Ulid::from_string(a).map_err(|_| ApiError::invalid("after must be a document ID."))?),
        None => None,
    };

    let page = engine.list_documents(&tess, after, limit).await?;
    Ok(Json(json!({
        "documents": docs_json(&page.documents),
        "next": page.next.map(|id| id.to_string()),
    }))
    .into_response())
}

/// Count documents.
pub async fn count_docs(Path(tess): Path<String>, State(engine): Engine) -> ApiResult {
    existing_tessellation(&engine, &tess)?;
    Ok(Json(json!({ "count": engine.count_documents(&tess).await? })).into_response())
}

/// Get a document.
pub async fn get_doc(Path((tess, id)): Path<(String, String)>, State(engine): Engine) -> ApiResult {
    user_tessellation(&engine, &tess)?;
    match engine.get_document(&tess, &id).await? {
        Some(doc) => Ok(Json(doc.to_api_json()).into_response()),
        None => Err(ApiError::not_found(format!("Document {} not found in '{}'.", id, tess))),
    }
}

/// Insert a document. Returns 201 with the stored document.
pub async fn insert_doc(
    Path(tess): Path<String>,
    params: Result<Query<WriteParams>, QueryRejection>,
    State(engine): Engine,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    let (Query(params), Json(body)) = (params?, body?);
    user_tessellation(&engine, &tess)?;
    let idem = idempotency(&headers, &method, &uri, Some(&body))?;

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
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    let (Query(params), Json(body)) = (params?, body?);
    user_tessellation(&engine, &tess)?;
    let idem = idempotency(&headers, &method, &uri, Some(&body))?;
    let outcome = engine.replace_document(&tess, &id, body, params.expiry(), idem).await?;
    Ok(respond(StatusCode::OK, outcome.value.to_api_json(), outcome.replayed))
}

/// Merge fields into a document (a `null` field removes it).
pub async fn patch_doc(
    Path((tess, id)): Path<(String, String)>,
    params: Result<Query<WriteParams>, QueryRejection>,
    State(engine): Engine,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    let (Query(params), Json(body)) = (params?, body?);
    user_tessellation(&engine, &tess)?;
    let idem = idempotency(&headers, &method, &uri, Some(&body))?;
    let outcome = engine.patch_document(&tess, &id, body, params.expiry(), idem).await?;
    Ok(respond(StatusCode::OK, outcome.value.to_api_json(), outcome.replayed))
}

/// Delete a document. Returns 204, or 404 if it doesn't exist.
pub async fn delete_doc(
    Path((tess, id)): Path<(String, String)>,
    State(engine): Engine,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult {
    user_tessellation(&engine, &tess)?;
    let idem = idempotency(&headers, &method, &uri, None)?;
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
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    let (Query(params), Json(body)) = (params?, body?);
    user_tessellation(&engine, &tess)?;
    let idem = idempotency(&headers, &method, &uri, Some(&body))?;
    let outcome = engine.insert_documents(&tess, bulk_items(body)?, params.expiry(), idem).await?;
    Ok(respond(StatusCode::CREATED, ids_json(&outcome.value), outcome.replayed))
}

/// Replace many documents atomically. Each item must include its `id`.
pub async fn bulk_replace(
    Path(tess): Path<String>,
    params: Result<Query<WriteParams>, QueryRejection>,
    State(engine): Engine,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    let (Query(params), Json(body)) = (params?, body?);
    user_tessellation(&engine, &tess)?;
    let idem = idempotency(&headers, &method, &uri, Some(&body))?;
    let targets = bulk_targets(bulk_items(body)?)?;
    let outcome = engine.replace_documents(&tess, targets, params.expiry(), idem).await?;
    Ok(respond(StatusCode::OK, ids_json(&outcome.value), outcome.replayed))
}

/// Merge-patch many documents atomically. Each item must include its `id`.
pub async fn bulk_patch(
    Path(tess): Path<String>,
    params: Result<Query<WriteParams>, QueryRejection>,
    State(engine): Engine,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    let (Query(params), Json(body)) = (params?, body?);
    user_tessellation(&engine, &tess)?;
    let idem = idempotency(&headers, &method, &uri, Some(&body))?;
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
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    let (Query(params), Json(body)) = (params?, body?);
    existing_tessellation(&engine, &tess)?;
    let idem = idempotency(&headers, &method, &uri, Some(&body))?;
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
pub async fn list_users(State(engine): Engine) -> ApiResult {
    Ok(Json(json!({ "users": users::list_users(&engine).await? })).into_response())
}

/// Create a user: `{"login", "password", "email_address", "roles": [{"name", "permissions": [...]}]}`.
pub async fn create_user(
    State(engine): Engine,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    let Json(body) = body?;
    let idem = idempotency(&headers, &method, &uri, Some(&body))?;
    let input: NewUser = serde_json::from_value(body).map_err(|e| ApiError::invalid(e.to_string()))?;
    let outcome = users::create_user(&engine, input, idem).await?;
    let location = format!("/users/{}", outcome.value.id);
    Ok(with_location(user_response(outcome, StatusCode::CREATED), &location))
}

/// Get a user by ID or login.
pub async fn get_user(Path(user): Path<String>, State(engine): Engine) -> ApiResult {
    match users::get_user(&engine, &user).await? {
        Some(view) => Ok(Json(view).into_response()),
        None => Err(ApiError::not_found(format!("User '{}' not found.", user))),
    }
}

async fn change_user(
    engine: &HexDBEngine,
    user: &str,
    full_replace: bool,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    let Json(body) = body?;
    let idem = idempotency(headers, method, uri, Some(&body))?;
    let changes: UserChanges = serde_json::from_value(body).map_err(|e| ApiError::invalid(e.to_string()))?;
    let outcome = users::update_user(engine, user, changes, full_replace, idem).await?;
    Ok(user_response(outcome, StatusCode::OK))
}

/// Replace a user's editable fields (`email_address` and `roles` required).
pub async fn replace_user(
    Path(user): Path<String>,
    State(engine): Engine,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    change_user(&engine, &user, true, &method, &uri, &headers, body).await
}

/// Change some of a user's fields.
pub async fn patch_user(
    Path(user): Path<String>,
    State(engine): Engine,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    change_user(&engine, &user, false, &method, &uri, &headers, body).await
}

/// Delete a user.
pub async fn delete_user(
    Path(user): Path<String>,
    State(engine): Engine,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult {
    let idem = idempotency(&headers, &method, &uri, None)?;
    let outcome = users::delete_user(&engine, &user, idem).await?;
    if !outcome.value {
        return Err(ApiError::not_found(format!("User '{}' not found.", user)));
    }
    Ok(no_content(outcome.replayed))
}

/// List roles.
pub async fn list_roles(State(engine): Engine) -> ApiResult {
    Ok(Json(json!({ "roles": users::list_roles(&engine).await? })).into_response())
}

/// Get a role by name.
pub async fn get_role(Path(name): Path<String>, State(engine): Engine) -> ApiResult {
    match users::get_role(&engine, &name).await? {
        Some(role) => Ok(Json(role).into_response()),
        None => Err(ApiError::not_found(format!("Role '{}' not found.", name))),
    }
}
