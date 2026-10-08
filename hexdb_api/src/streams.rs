// HexDB API: streams (publish/subscribe; see hexdb_core::streams)
//
//   GET    /streams                                   streams you can read
//   POST   /streams                                   create (manage permission on stream:<name>)
//   GET    /streams/{name}                            configuration, delivery status, consumer groups
//   PUT    /streams/{name}                            change the configuration
//   DELETE /streams/{name}                            delete it with its messages
//   POST   /streams/{name}/messages                   publish one message or an array (write)
//   GET    /streams/{name}/messages?after=&group=&limit=&wait=   read in order (read); long-polls with wait
//   POST   /streams/{name}/groups/{group}/commit      {"offset"}: a consumer group's position (read)
//   GET    /streams/{name}/subscribe?after=&group=    Server-Sent Events, live (read)

use crate::auth::Auth;
use crate::handlers::{ApiError, ApiResult, Engine};
use axum::{
    extract::{rejection::{JsonRejection, QueryRejection}, Path, Query},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use futures::stream::{self, Stream};
use hexdb_core::{
    streams::{self, NewMessage, StreamConfig},
    Permission, Principal,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{convert::Infallible, time::Duration};
use ulid::Ulid;

const MAX_WAIT_SECONDS: u64 = 60;

fn require(principal: &Principal, permission: Permission, name: &str) -> Result<(), ApiError> {
    principal.require(permission, &streams::resource(name)).map_err(ApiError::from)
}

fn parse_offset(text: Option<&str>) -> Result<Option<Ulid>, ApiError> {
    match text.filter(|t| !t.is_empty()) {
        Some(t) => Ulid::from_string(t).map(Some).map_err(|_| ApiError::invalid("offset must be a message offset (a ULID).")),
        None => Ok(None),
    }
}

/// Streams the caller can read.
pub async fn list(axum::extract::State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    let list: Vec<StreamConfig> = engine.list_streams().await?.into_iter().filter(|s| principal.can(Permission::Read, &streams::resource(&s.name))).collect();
    Ok(Json(json!({ "streams": list })).into_response())
}

/// A stream's sources copy every change of their tessellations, so whoever
/// configures them needs unrestricted read access there (no row filter or
/// field mask), or a stream could reveal what their role hides.
fn require_sources(principal: &hexdb_core::Principal, config: &StreamConfig) -> Result<(), ApiError> {
    for source in &config.sources {
        principal.require(Permission::Read, &source.tessellation)?;
        if !principal.unrestricted(&source.tessellation, hexdb_core::Action::Read) {
            return Err(ApiError::from(anyhow::Error::from(hexdb_core::EngineError::Forbidden(format!(
                "Your role's access to '{}' is restricted, so it can't be a stream source.",
                source.tessellation
            )))));
        }
    }
    Ok(())
}

/// Create a stream.
pub async fn create(axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<StreamConfig>, JsonRejection>) -> ApiResult {
    let Json(config) = body?;
    require(&principal, Permission::Manage, &config.name)?;
    require_sources(&principal, &config)?;
    let saved = engine.save_stream(config, &principal.login, true).await?;
    engine.audit(&principal.login, "stream.create", &saved.name, json!({ "sources": saved.sources.len(), "destinations": saved.destinations.len() })).await;
    Ok((StatusCode::CREATED, Json(json!(saved))).into_response())
}

/// A stream's configuration, delivery status and consumer groups.
pub async fn get(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    require(&principal, Permission::Read, &name)?;
    let config = engine.get_stream(&name).await?.ok_or_else(|| ApiError::from(anyhow::Error::from(hexdb_core::EngineError::NotFound(format!("Stream '{}' not found.", name)))))?;
    let status = engine.stream_tasks.status.lock().unwrap().clone();
    let task = |kind: &str, i: usize| status.get(&format!("{}/{}/{}", name, kind, i)).cloned().unwrap_or_default();
    Ok(Json(json!({
        "stream": config,
        "sources": (0..config.sources.len()).map(|i| task("source", i)).collect::<Vec<_>>(),
        "destinations": (0..config.destinations.len()).map(|i| task("destination", i)).collect::<Vec<_>>(),
        "groups": engine.stream_groups(&name).await?,
    }))
    .into_response())
}

/// Replace a stream's configuration.
pub async fn update(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<StreamConfig>, JsonRejection>) -> ApiResult {
    require(&principal, Permission::Manage, &name)?;
    let Json(mut config) = body?;
    config.name = name.clone();
    require_sources(&principal, &config)?;
    let saved = engine.save_stream(config, &principal.login, false).await?;
    engine.audit(&principal.login, "stream.update", &name, json!({})).await;
    Ok(Json(json!(saved)).into_response())
}

/// Delete a stream and its messages.
pub async fn delete(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    require(&principal, Permission::Manage, &name)?;
    if !engine.delete_stream(&name).await? {
        return Err(ApiError::from(anyhow::Error::from(hexdb_core::EngineError::NotFound(format!("Stream '{}' not found.", name)))));
    }
    engine.audit(&principal.login, "stream.delete", &name, json!({})).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Publish: `{"payload": ..., "key"?, "headers"?}` or an array of those.
pub async fn publish(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<Value>, JsonRejection>) -> ApiResult {
    require(&principal, Permission::Write, &name)?;
    let Json(body) = body?;
    let messages: Vec<NewMessage> = match body {
        Value::Array(items) => items.into_iter().map(serde_json::from_value).collect::<Result<_, _>>(),
        single => serde_json::from_value(single).map(|m| vec![m]),
    }
    .map_err(|e| ApiError::invalid(format!("A message is {{\"payload\": ..., \"key\"?, \"headers\"?}}: {}", e)))?;
    let offsets = engine.publish(&name, messages, &principal.login).await?;
    Ok((StatusCode::CREATED, Json(json!({ "offsets": offsets }))).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    pub after: Option<String>,
    /// Start after this consumer group's committed offset (when `after` isn't given).
    pub group: Option<String>,
    pub limit: Option<usize>,
    pub wait: Option<u64>,
}

async fn start_offset(engine: &hexdb_core::HexDBEngine, name: &str, params: &ReadParams) -> Result<Option<Ulid>, ApiError> {
    match parse_offset(params.after.as_deref())? {
        Some(offset) => Ok(Some(offset)),
        None => match params.group.as_deref().filter(|g| !g.is_empty()) {
            Some(group) => Ok(engine.group_offset(name, group).await?),
            None => Ok(None),
        },
    }
}

/// Read messages in order; with `wait`, long-poll until one arrives.
pub async fn read(
    Path(name): Path<String>,
    params: Result<Query<ReadParams>, QueryRejection>,
    axum::extract::State(engine): Engine,
    Auth(principal): Auth,
) -> ApiResult {
    require(&principal, Permission::Read, &name)?;
    let Query(params) = params?;
    let after = start_offset(&engine, &name, &params).await?;
    let limit = params.limit.unwrap_or(100).clamp(1, streams::MAX_BATCH);
    let (mut messages, mut next) = engine.read_stream(&name, after, limit).await?;
    let wait = params.wait.unwrap_or(0).min(MAX_WAIT_SECONDS);
    if messages.is_empty() && wait > 0 {
        engine.wait_for_messages(&name, after, Duration::from_secs(wait)).await;
        (messages, next) = engine.read_stream(&name, after, limit).await?;
    }
    Ok(Json(json!({ "messages": messages, "next": next })).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Commit {
    pub offset: String,
}

/// Commit a consumer group's offset (it only moves forward).
pub async fn commit(Path((name, group)): Path<(String, String)>, axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<Commit>, JsonRejection>) -> ApiResult {
    require(&principal, Permission::Read, &name)?;
    let Json(input) = body?;
    let offset = parse_offset(Some(&input.offset))?.ok_or_else(|| ApiError::invalid("offset is required."))?;
    engine.commit_offset(&name, &group, offset).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Messages as Server-Sent Events (`event: message`, `id: <offset>`), from
/// `after` (or the group's offset, or now) and live after that.
pub async fn subscribe(
    Path(name): Path<String>,
    params: Result<Query<ReadParams>, QueryRejection>,
    headers: HeaderMap,
    axum::extract::State(engine): Engine,
    Auth(principal): Auth,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    require(&principal, Permission::Read, &name)?;
    let Query(params) = params?;
    let resume = headers.get("last-event-id").and_then(|v| v.to_str().ok()).and_then(|v| Ulid::from_string(v.trim()).ok());
    let start = match resume {
        Some(r) => Some(r),
        None if params.after.is_none() && params.group.is_none() => {
            // From now: the newest message's offset.
            let mut last = None;
            loop {
                let (page, next) = engine.read_stream(&name, last, streams::MAX_BATCH).await?;
                if page.is_empty() {
                    break;
                }
                last = next.and_then(|n| Ulid::from_string(&n).ok());
            }
            last
        }
        None => start_offset(&engine, &name, &params).await?,
    };
    engine.read_stream(&name, None, 1).await?;
    let credential = crate::auth::request_credential(&headers).unwrap_or_default();
    struct State {
        engine: std::sync::Arc<hexdb_core::HexDBEngine>,
        name: String,
        after: Option<Ulid>,
        queue: std::collections::VecDeque<Value>,
        credential: String,
        checked: std::time::Instant,
        done: bool,
    }
    let state = State { engine, name, after: start, queue: Default::default(), credential, checked: std::time::Instant::now(), done: false };
    let events = stream::unfold(state, |mut s| async move {
        if s.done {
            return None;
        }
        loop {
            if let Some(message) = s.queue.pop_front() {
                let id = message["offset"].as_str().unwrap_or_default().to_string();
                return Some((Ok(Event::default().event("message").id(id).data(message.to_string())), s));
            }
            // Re-check the credentials now and then: a long stream outlives a revocation otherwise.
            if s.checked.elapsed() >= Duration::from_secs(30) {
                let ok = matches!(hexdb_core::auth::authenticate(&s.engine, &s.credential).await, Ok(Some(p)) if p.can(Permission::Read, &streams::resource(&s.name)));
                if !ok {
                    s.done = true;
                    return Some((Ok(Event::default().event("unauthorized").data("{\"message\":\"The credentials are no longer valid.\"}")), s));
                }
                s.checked = std::time::Instant::now();
            }
            match s.engine.read_stream(&s.name, s.after, 100).await {
                Ok((messages, next)) if !messages.is_empty() => {
                    s.after = next.and_then(|n| Ulid::from_string(&n).ok());
                    s.queue.extend(messages);
                }
                Ok(_) => s.engine.wait_for_messages(&s.name, s.after, Duration::from_secs(15)).await,
                Err(_) => return None,
            }
        }
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}
