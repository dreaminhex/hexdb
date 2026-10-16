// HexDB API: live feeds over Server-Sent Events or WebSocket
//
// The change feed (`/changes/stream`, `/changes/ws`) and stream subscriptions
// (`/streams/{name}/subscribe`, `/streams/{name}/ws`) build one stream of
// `LiveEvent`s, with the same permission checks, row filters, field masks and
// periodic re-authentication either way; this module sends it as SSE events
// or as WebSocket text frames: `{"event": "change", "id": "42", "data": {...}}`.
//
// A browser sends cookies with a WebSocket handshake from any site, and the
// handshake is a GET, so the CSRF check for changes doesn't cover it: a
// cookie-authenticated upgrade must come from this server's own pages
// (cross-site WebSocket hijacking). Bearer tokens can't be sent by another
// site's page, so they need no check.

use crate::handlers::ApiError;
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    http::HeaderMap,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
};
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::{convert::Infallible, time::Duration};

/// One event of a live feed.
pub struct LiveEvent {
    pub event: &'static str,
    pub id: Option<String>,
    pub data: Value,
}

impl LiveEvent {
    pub fn new(event: &'static str, id: Option<String>, data: Value) -> Self {
        LiveEvent { event, id, data }
    }

    fn sse(self) -> Event {
        // Axum writes fields in the order they're set: the id goes before the data.
        let event = Event::default().event(self.event);
        let event = match self.id {
            Some(id) => event.id(id),
            None => event,
        };
        event.data(self.data.to_string())
    }
}

/// Send a feed as Server-Sent Events.
pub fn sse(events: impl Stream<Item = LiveEvent> + Send + 'static) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(events.map(|e| Ok(e.sse()))).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// Refuse a cookie-authenticated WebSocket upgrade from another site.
pub fn check_origin(headers: &HeaderMap) -> Result<(), ApiError> {
    let by_cookie = crate::auth::session_cookie(headers).is_some() && headers.get(axum::http::header::AUTHORIZATION).is_none();
    if by_cookie && !crate::auth::same_origin(headers) {
        return Err(ApiError::new(axum::http::StatusCode::FORBIDDEN, "cross_origin", "WebSocket connections signed in with a cookie must come from the HexDB admin UI."));
    }
    Ok(())
}

/// Send a feed over a WebSocket.
pub fn websocket(upgrade: WebSocketUpgrade, events: impl Stream<Item = LiveEvent> + Send + 'static) -> Response {
    upgrade.on_upgrade(move |socket| forward(socket, events)).into_response()
}

async fn forward(mut socket: WebSocket, events: impl Stream<Item = LiveEvent> + Send + 'static) {
    let mut events = Box::pin(events);
    let mut ping = tokio::time::interval(Duration::from_secs(15));
    ping.tick().await;
    loop {
        tokio::select! {
            event = events.next() => {
                let Some(event) = event else { break };
                let frame = json!({ "event": event.event, "id": event.id, "data": event.data }).to_string();
                if socket.send(Message::Text(frame.into())).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                // The feed is one-way; the client's only say is to close it.
                match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}
