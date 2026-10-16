// HexDB API: gRPC
//
// The gRPC service (proto/hexdb/v1/hexdb.proto) shares the REST port: a
// request with `content-type: application/grpc` goes to it, anything else to
// the REST router. Each call becomes the matching REST request and runs
// through the REST router with all its middleware (client address,
// authentication, permissions, row filters, field masks, forwarding writes to
// the Overseer, quotas, rate limits, audit and the request log), so gRPC has
// no permission logic of its own to drift from REST's.
//
// Credentials come only from the `authorization` metadata. Cookies (and
// Origin/Referer) are dropped, so a browser can't make a call with a
// visitor's session.

use axum::{
    body::Body,
    extract::{ConnectInfo, Request},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::Response,
    Router,
};
use futures::{Stream, StreamExt};
use serde_json::Value;
use std::{net::SocketAddr, pin::Pin};
use tonic::{metadata::MetadataValue, Status};
use tower::ServiceExt;

pub mod pb {
    tonic::include_proto!("hexdb.v1");
}

use pb::hex_db_server::{HexDb, HexDbServer};

/// The largest REST response a unary call returns (bigger downloads: use REST).
const MAX_REPLY: usize = 64 * 1024 * 1024;

/// Metadata never passed to the REST request.
const DROPPED: &[&str] = &["cookie", "origin", "referer", "host", "content-type", "content-length", "transfer-encoding", "te", "connection", "upgrade"];

/// The REST router with the gRPC service in front of it.
pub fn with_grpc(rest: Router, body_limit: usize) -> Router {
    let service = HexDbServer::new(Grpc { rest: rest.clone() }).max_decoding_message_size(body_limit.saturating_add(64 * 1024)).max_encoding_message_size(MAX_REPLY + 1024);
    let grpc = tonic::service::Routes::new(service).into_axum_router();
    Router::new().fallback_service(tower::service_fn(move |request: Request| {
        let (grpc, rest) = (grpc.clone(), rest.clone());
        async move {
            if is_grpc(request.headers()) {
                grpc.oneshot(request).await
            } else {
                rest.oneshot(request).await
            }
        }
    }))
}

fn is_grpc(headers: &HeaderMap) -> bool {
    headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("application/grpc"))
}

#[derive(Clone)]
struct Grpc {
    rest: Router,
}

/// Percent-encode a path segment.
fn segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn with_query(path: String, query: &str) -> String {
    let query = query.trim_start_matches('?');
    if query.is_empty() {
        path
    } else {
        format!("{}?{}", path, query)
    }
}

fn required(value: &str, what: &str) -> Result<(), Status> {
    if value.is_empty() {
        return Err(Status::invalid_argument(format!("{} is required.", what)));
    }
    Ok(())
}

impl Grpc {
    /// Run a REST request as the caller.
    async fn rest<T>(&self, request: &tonic::Request<T>, method: Method, path: &str, json: &str) -> Result<Response, Status> {
        if !path.starts_with('/') || path.starts_with("//") {
            return Err(Status::invalid_argument("The path must start with a single '/'."));
        }
        let body = if json.trim().is_empty() { Body::empty() } else { Body::from(json.to_string()) };
        let mut rest = Request::builder().method(method).uri(path).body(body).map_err(|e| Status::invalid_argument(format!("Bad path: {}", e)))?;
        let headers = rest.headers_mut();
        for (name, value) in request.metadata().clone().into_headers().iter() {
            let n = name.as_str();
            if DROPPED.contains(&n) || n.starts_with("grpc-") || n.starts_with("sec-") {
                continue;
            }
            headers.append(name.clone(), value.clone());
        }
        if !json.trim().is_empty() {
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        if let Some(peer) = request.extensions().get::<ConnectInfo<SocketAddr>>() {
            rest.extensions_mut().insert(*peer);
        }
        Ok(self.rest.clone().oneshot(rest).await.unwrap_or_else(|never| match never {}))
    }

    async fn unary<T>(&self, request: &tonic::Request<T>, method: Method, path: &str, json: &str) -> Result<tonic::Response<pb::Reply>, Status> {
        let response = self.rest(request, method, path, json).await?;
        let status = response.status();
        let content_type = response.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        let retry_after = response.headers().get(header::RETRY_AFTER).cloned();
        let bytes = axum::body::to_bytes(response.into_body(), MAX_REPLY)
            .await
            .map_err(|_| Status::resource_exhausted("The reply is too large for gRPC; use the REST API for large downloads."))?;
        if !status.is_success() {
            return Err(error_status(status, &bytes, retry_after));
        }
        let mut reply = pb::Reply { status: status.as_u16() as u32, ..Default::default() };
        if content_type.starts_with("application/json") {
            reply.json = String::from_utf8_lossy(&bytes).into_owned();
        } else {
            reply.body = bytes.to_vec();
            reply.content_type = content_type;
        }
        Ok(tonic::Response::new(reply))
    }

    /// A REST Server-Sent Events feed as a stream of events.
    async fn events<T>(&self, request: &tonic::Request<T>, path: &str) -> Result<EventStream, Status> {
        let response = self.rest(request, Method::GET, path, "").await?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = response.headers().get(header::RETRY_AFTER).cloned();
            let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap_or_default();
            return Err(error_status(status, &bytes, retry_after));
        }
        Ok(Box::pin(sse_events(response.into_body())))
    }
}

type EventStream = Pin<Box<dyn Stream<Item = Result<pb::Event, Status>> + Send + 'static>>;

/// Parse a Server-Sent Events body: one `pb::Event` per event; comments (keep-alives) are skipped.
fn sse_events(body: Body) -> impl Stream<Item = Result<pb::Event, Status>> + Send + 'static {
    futures::stream::unfold((body.into_data_stream(), String::new(), false), |(mut chunks, mut buffer, done)| async move {
        loop {
            if let Some(end) = buffer.find("\n\n") {
                let block: String = buffer.drain(..end + 2).collect();
                let mut event = pb::Event::default();
                let mut data: Vec<&str> = Vec::new();
                for line in block.lines() {
                    let (field, value) = line.split_once(':').unwrap_or((line, ""));
                    let value = value.strip_prefix(' ').unwrap_or(value);
                    match field {
                        "event" => event.event = value.to_string(),
                        "id" => event.id = value.to_string(),
                        "data" => data.push(value),
                        _ => {}
                    }
                }
                if event.event.is_empty() && data.is_empty() {
                    continue;
                }
                event.json = data.join("\n");
                return Some((Ok(event), (chunks, buffer, done)));
            }
            if done {
                return None;
            }
            match chunks.next().await {
                Some(Ok(bytes)) => buffer.push_str(&String::from_utf8_lossy(&bytes).replace("\r\n", "\n")),
                Some(Err(_)) => return Some((Err(Status::unavailable("The feed ended unexpectedly.")), (chunks, buffer, true))),
                None => return None,
            }
        }
    })
}

/// A REST error as a gRPC status, with the REST error code in the `hexdb-error` trailer.
fn error_status(status: StatusCode, body: &[u8], retry_after: Option<HeaderValue>) -> Status {
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let error = parsed.as_ref().and_then(|v| v.get("error"));
    let message = error.and_then(|e| e["message"].as_str()).map(String::from).unwrap_or_else(|| status.canonical_reason().unwrap_or("Request failed").to_string());
    let code = match status.as_u16() {
        400 | 405 | 415 | 422 => tonic::Code::InvalidArgument,
        401 => tonic::Code::Unauthenticated,
        403 => tonic::Code::PermissionDenied,
        404 => tonic::Code::NotFound,
        409 => tonic::Code::AlreadyExists,
        410 => tonic::Code::OutOfRange,
        412 | 428 => tonic::Code::FailedPrecondition,
        413 | 429 | 507 => tonic::Code::ResourceExhausted,
        501 => tonic::Code::Unimplemented,
        503 => tonic::Code::Unavailable,
        504 => tonic::Code::DeadlineExceeded,
        _ => tonic::Code::Internal,
    };
    let mut result = Status::new(code, message);
    if let Some(code) = error.and_then(|e| e["code"].as_str()).and_then(|c| MetadataValue::try_from(c).ok()) {
        result.metadata_mut().insert("hexdb-error", code);
    }
    if let Some(value) = retry_after.and_then(|v| MetadataValue::try_from(v.as_bytes()).ok()) {
        result.metadata_mut().insert("retry-after", value);
    }
    result
}

fn method(name: &str) -> Result<Method, Status> {
    match name.to_ascii_uppercase().as_str() {
        "GET" => Ok(Method::GET),
        "POST" => Ok(Method::POST),
        "PUT" => Ok(Method::PUT),
        "PATCH" => Ok(Method::PATCH),
        "DELETE" => Ok(Method::DELETE),
        _ => Err(Status::invalid_argument("The method must be GET, POST, PUT, PATCH or DELETE.")),
    }
}

#[tonic::async_trait]
impl HexDb for Grpc {
    async fn get(&self, request: tonic::Request<pb::DocumentRef>) -> Result<tonic::Response<pb::Reply>, Status> {
        let r = request.get_ref();
        required(&r.tessellation, "tessellation")?;
        required(&r.id, "id")?;
        let path = with_query(format!("/{}/{}", segment(&r.tessellation), segment(&r.id)), &r.query);
        self.unary(&request, Method::GET, &path, "").await
    }

    async fn insert(&self, request: tonic::Request<pb::Write>) -> Result<tonic::Response<pb::Reply>, Status> {
        let r = request.get_ref();
        required(&r.tessellation, "tessellation")?;
        required(&r.json, "json")?;
        let path = with_query(format!("/{}", segment(&r.tessellation)), &r.query);
        self.unary(&request, Method::POST, &path, &r.json).await
    }

    async fn replace(&self, request: tonic::Request<pb::Write>) -> Result<tonic::Response<pb::Reply>, Status> {
        let r = request.get_ref();
        required(&r.tessellation, "tessellation")?;
        required(&r.id, "id")?;
        required(&r.json, "json")?;
        let path = with_query(format!("/{}/{}", segment(&r.tessellation), segment(&r.id)), &r.query);
        self.unary(&request, Method::PUT, &path, &r.json).await
    }

    async fn patch(&self, request: tonic::Request<pb::Write>) -> Result<tonic::Response<pb::Reply>, Status> {
        let r = request.get_ref();
        required(&r.tessellation, "tessellation")?;
        required(&r.id, "id")?;
        required(&r.json, "json")?;
        let path = with_query(format!("/{}/{}", segment(&r.tessellation), segment(&r.id)), &r.query);
        self.unary(&request, Method::PATCH, &path, &r.json).await
    }

    async fn delete(&self, request: tonic::Request<pb::DocumentRef>) -> Result<tonic::Response<pb::Reply>, Status> {
        let r = request.get_ref();
        required(&r.tessellation, "tessellation")?;
        required(&r.id, "id")?;
        let path = with_query(format!("/{}/{}", segment(&r.tessellation), segment(&r.id)), &r.query);
        self.unary(&request, Method::DELETE, &path, "").await
    }

    async fn query(&self, request: tonic::Request<pb::Body>) -> Result<tonic::Response<pb::Reply>, Status> {
        let r = request.get_ref();
        required(&r.name, "name (the tessellation)")?;
        let json = if r.json.trim().is_empty() { "{}" } else { &r.json };
        self.unary(&request, Method::POST, &format!("/{}/_query", segment(&r.name)), json).await
    }

    async fn aggregate(&self, request: tonic::Request<pb::Body>) -> Result<tonic::Response<pb::Reply>, Status> {
        let r = request.get_ref();
        required(&r.name, "name (the tessellation)")?;
        required(&r.json, "json")?;
        self.unary(&request, Method::POST, &format!("/{}/_aggregate", segment(&r.name)), &r.json).await
    }

    async fn sql(&self, request: tonic::Request<pb::Body>) -> Result<tonic::Response<pb::Reply>, Status> {
        let r = request.get_ref();
        required(&r.json, "json")?;
        self.unary(&request, Method::POST, "/sql", &r.json).await
    }

    async fn publish(&self, request: tonic::Request<pb::Body>) -> Result<tonic::Response<pb::Reply>, Status> {
        let r = request.get_ref();
        required(&r.name, "name (the stream)")?;
        required(&r.json, "json")?;
        self.unary(&request, Method::POST, &format!("/streams/{}/messages", segment(&r.name)), &r.json).await
    }

    async fn call(&self, request: tonic::Request<pb::CallRequest>) -> Result<tonic::Response<pb::Reply>, Status> {
        let r = request.get_ref();
        let method = method(&r.method)?;
        self.unary(&request, method, &r.path, &r.json).await
    }

    type ChangesStream = EventStream;

    async fn changes(&self, request: tonic::Request<pb::ChangesRequest>) -> Result<tonic::Response<Self::ChangesStream>, Status> {
        let r = request.get_ref();
        let mut query = Vec::new();
        if let Some(after) = r.after {
            query.push(format!("after={}", after));
        }
        if !r.tessellation.is_empty() {
            query.push(format!("tessellation={}", segment(&r.tessellation)));
        }
        let path = with_query("/changes/stream".into(), &query.join("&"));
        Ok(tonic::Response::new(self.events(&request, &path).await?))
    }

    type SubscribeStream = EventStream;

    async fn subscribe(&self, request: tonic::Request<pb::SubscribeRequest>) -> Result<tonic::Response<Self::SubscribeStream>, Status> {
        let r = request.get_ref();
        required(&r.stream, "stream")?;
        let mut query = Vec::new();
        if !r.after.is_empty() {
            query.push(format!("after={}", segment(&r.after)));
        }
        if !r.group.is_empty() {
            query.push(format!("group={}", segment(&r.group)));
        }
        let path = with_query(format!("/streams/{}/subscribe", segment(&r.stream)), &query.join("&"));
        Ok(tonic::Response::new(self.events(&request, &path).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_encoded() {
        assert_eq!(segment("notes"), "notes");
        assert_eq!(segment("a/b?c"), "a%2Fb%3Fc");
        assert_eq!(segment("../users"), "..%2Fusers");
    }

    #[tokio::test]
    async fn server_sent_events_are_parsed() {
        let body = Body::from(": keep-alive\n\nevent: change\nid: 7\ndata: {\"a\":1}\n\nevent: lagged\r\ndata: {}\r\n\r\n");
        let events: Vec<pb::Event> = sse_events(body).map(|e| e.unwrap()).collect().await;
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].event.as_str(), events[0].id.as_str(), events[0].json.as_str()), ("change", "7", "{\"a\":1}"));
        assert_eq!(events[1].event, "lagged");
    }

    #[test]
    fn errors_map_to_grpc_codes() {
        let status = error_status(StatusCode::FORBIDDEN, br#"{"error":{"code":"forbidden","message":"No."}}"#, None);
        assert_eq!(status.code(), tonic::Code::PermissionDenied);
        assert_eq!(status.message(), "No.");
        assert_eq!(status.metadata().get("hexdb-error").unwrap(), "forbidden");
    }
}
