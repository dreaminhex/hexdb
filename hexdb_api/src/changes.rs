// HexDB API: change feed
//
//   GET /changes?after=<seq>&tessellation=<name>&limit=<n>&wait=<seconds>
//       Changes after a sequence number, oldest first:
//       {"changes": [...], "last_seq": <cursor>, "available_after": <seq>}.
//       Pass `last_seq` back as `after` to continue. Without `after`, starts
//       from now. With `wait`, blocks up to that many seconds (max 60) until a
//       change arrives (long polling).
//
//   GET /changes/stream?after=<seq>&tessellation=<name>
//       The same changes as Server-Sent Events (`event: change`, `id: <seq>`),
//       live. Reconnecting clients resume from the `Last-Event-ID` header.
//
// Both return 410 (`history_expired`) when the requested position is older
// than the changes kept in memory; re-read the data and start from `last_seq`
// of a fresh request. System tessellations (users, roles, ...) are left out.

use crate::handlers::{ApiError, ApiResult, Engine};
use axum::{
    extract::{rejection::QueryRejection, Query},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use futures::stream::{self, Stream, StreamExt};
use hexdb_core::{engine::HexDBEngine, Change, ChangeKind, HistoryExpired};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::broadcast::{self, error::RecvError};

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;
const MAX_WAIT_SECONDS: u64 = 60;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeParams {
    pub after: Option<u64>,
    pub tessellation: Option<String>,
    pub limit: Option<usize>,
    pub wait: Option<u64>,
}

fn expired(e: HistoryExpired) -> ApiError {
    ApiError::new(
        StatusCode::GONE,
        "history_expired",
        format!(
            "Changes before sequence {} are no longer kept. Re-read the data you need, then follow changes after {}.",
            e.available_after + 1,
            e.available_after
        ),
    )
}

/// True if the public feed shows this change.
fn visible(engine: &HexDBEngine, change: &Change, tessellation: Option<&str>) -> bool {
    if tessellation.is_some_and(|t| t != change.tessellation) {
        return false;
    }
    // A dropped tessellation is no longer in the catalog; judge it by name.
    if change.kind == ChangeKind::DropTessellation {
        return !change.tessellation.starts_with('_');
    }
    !engine.is_system_tessellation(&change.tessellation)
}

/// Changes after a sequence number (see the module docs).
pub async fn changes(params: Result<Query<ChangeParams>, QueryRejection>, axum::extract::State(engine): Engine) -> ApiResult {
    let Query(params) = params?;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let tessellation = params.tessellation.as_deref().filter(|t| !t.is_empty());
    let wait = Duration::from_secs(params.wait.unwrap_or(0).min(MAX_WAIT_SECONDS));
    let mut cursor = params.after.unwrap_or_else(|| engine.changes.published_seq());
    let deadline = tokio::time::Instant::now() + wait;

    let respond = |changes: Vec<Value>, cursor: u64| {
        Ok(Json(json!({
            "changes": changes,
            "last_seq": cursor,
            "available_after": engine.changes.available_after(),
        }))
        .into_response())
    };
    loop {
        // Subscribe before reading so a change published in between isn't missed.
        let (backlog, mut receiver) = engine.changes.follow(cursor).map_err(expired)?;
        let mut out = Vec::new();
        for change in &backlog {
            if out.len() == limit {
                break;
            }
            cursor = change.seq;
            if visible(&engine, change, tessellation) {
                out.push(change.to_api_json());
            }
        }
        if !out.is_empty() || wait.is_zero() {
            return respond(out, cursor);
        }
        // Long poll: wait for anything new, then look again.
        if tokio::time::timeout_at(deadline, receiver.recv()).await.is_err() {
            return respond(out, cursor);
        }
    }
}

/// Live changes as Server-Sent Events.
pub async fn change_stream(
    params: Result<Query<ChangeParams>, QueryRejection>,
    headers: HeaderMap,
    axum::extract::State(engine): Engine,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let Query(params) = params?;
    if params.limit.is_some() || params.wait.is_some() {
        return Err(ApiError::invalid("limit and wait don't apply to the stream."));
    }
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let after = resume.or(params.after).unwrap_or_else(|| engine.changes.published_seq());
    let (backlog, receiver) = engine.changes.follow(after).map_err(expired)?;
    let tessellation = params.tessellation.filter(|t| !t.is_empty());

    let event = |change: &Change| {
        Event::default()
            .event("change")
            .id(change.seq.to_string())
            .data(change.to_api_json().to_string())
    };

    let filter_engine = engine.clone();
    let filter_tess = tessellation.clone();
    let backlog = stream::iter(
        backlog
            .into_iter()
            .filter(move |c| visible(&filter_engine, c, filter_tess.as_deref()))
            .map(move |c| Ok(event(&c)))
            .collect::<Vec<_>>(),
    );

    struct Live {
        engine: Arc<HexDBEngine>,
        receiver: broadcast::Receiver<Arc<Change>>,
        tessellation: Option<String>,
        done: bool,
    }
    let live = stream::unfold(Live { engine, receiver, tessellation, done: false }, move |mut s| async move {
        if s.done {
            return None;
        }
        loop {
            match s.receiver.recv().await {
                Ok(change) => {
                    if visible(&s.engine, &change, s.tessellation.as_deref()) {
                        return Some((Ok(event(&change)), s));
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    // The client fell too far behind; tell it and close so it resumes from its last ID.
                    s.done = true;
                    let notice = Event::default().event("lagged").data("{\"message\":\"Fell behind the change feed; reconnect to resume.\"}");
                    return Some((Ok(notice), s));
                }
                Err(RecvError::Closed) => return None,
            }
        }
    });

    Ok(Sse::new(backlog.chain(live)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}
