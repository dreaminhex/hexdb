// HexDB API: forwarding writes from replicas to the Overseer
//
// Only the Overseer accepts writes. With `replication.forward_writes` (the
// default), a replica passes a write request it receives on to the Overseer
// and returns the Overseer's response, so clients can use any hex. The
// replica authenticates the request first (sessions and API keys are valid on
// every hex) and checks CSRF; the Overseer authenticates it again.
//
// The Overseer believes the forwarded client address only with a valid
// lattice signature over the method, path and address, so a client can't
// claim another address by sending the header itself. Reads, sign-in and
// sign-out, this hex's own maintenance and settings (flush, compact, scrub,
// backup, shutdown, `PUT /settings`), analyzer previews and hex-to-hex
// endpoints are always handled locally. GraphQL requests are forwarded only when they
// contain a mutation.
//
// File uploads (`/_files/`, and parts of resumable uploads) are streamed to the Overseer as they arrive, with
// no size limit or timeout here; other bodies are read first (up to
// `limits.max_request_mb`).
//
// Without a reachable Overseer, the request is handled locally, which refuses
// writes with 421 `read_only_replica`.

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use hexdb_core::{engine::HexDBEngine, network::discovery::ROLE_OVERSEER};
use std::{sync::Arc, time::Duration};
use tracing::{debug, warn};

/// The client address a replica forwarded a request for.
pub const FORWARDED_FOR_HEADER: &str = "x-hexdb-forwarded-for";
/// Lattice signature over the forwarded request's method, path and client address.
pub const FORWARD_SIGNATURE_HEADER: &str = "x-hexdb-forward-signature";
/// Response header naming the Overseer that handled a forwarded request.
pub const FORWARDED_TO_HEADER: &str = "x-hexdb-forwarded-to";

/// Marks a request a replica forwarded (verified by its signature).
#[derive(Clone, Copy)]
pub struct Forwarded;

/// What the forwarding signature covers.
pub fn signed_target(path_and_query: &str, client: &str) -> String {
    format!("{}#forwarded-for={}", path_and_query, client)
}

fn handled_locally(method: &Method, path: &str) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
        || path.starts_with("/lattice/")
        || path.starts_with("/ui")
        || matches!(path, "/auth/login" | "/auth/logout" | "/shutdown" | "/flush" | "/compact" | "/scrub" | "/backup" | "/settings" | "/analyzers/_analyze")
        || path.ends_with("/_query")
        || path.ends_with("/_aggregate")
        || path == "/sql"
        // MCP only reads; any hex answers.
        || path == "/mcp"
}

/// True if a GraphQL request body contains a mutation.
fn graphql_mutation(body: &[u8]) -> bool {
    let Ok(request) = serde_json::from_slice::<serde_json::Value>(body) else { return false };
    let Some(query) = request.get("query").and_then(|q| q.as_str()) else { return false };
    match async_graphql::parser::parse_query(query) {
        Ok(doc) => doc.operations.iter().any(|(_, op)| op.node.ty == async_graphql::parser::types::OperationType::Mutation),
        // Let the local server report the syntax error.
        Err(_) => false,
    }
}

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

fn copy_headers(from: &HeaderMap, to: &mut HeaderMap) {
    for (name, value) in from {
        let n = name.as_str();
        if HOP_BY_HOP.contains(&n) || n == FORWARDED_FOR_HEADER || n == FORWARD_SIGNATURE_HEADER {
            continue;
        }
        to.append(name.clone(), value.clone());
    }
}

/// The client used for forwarding, made once.
fn forward_client(engine: &HexDBEngine) -> Option<reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| match hexdb_core::replication::lattice_client(&engine.config, Duration::from_secs(300)) {
            Ok(client) => Some(client),
            Err(e) => {
                warn!("⚠️ Can't forward writes to the Overseer: {:#}", e);
                None
            }
        })
        .clone()
}

/// Stream a file upload to the Overseer.
async fn forward_upload(engine: &Arc<HexDBEngine>, overseer: hexdb_core::network::discovery::PeerHex, request: Request) -> Response {
    let Some(http) = forward_client(engine) else { return crate::handlers::ApiError::new(StatusCode::BAD_GATEWAY, "overseer_unreachable", "This hex can't reach the Overseer.").into_response() };
    let (parts, body) = request.into_parts();
    let client = crate::auth::client_address(&parts.extensions);
    let path_and_query = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| parts.uri.path().to_string());
    let url = format!("{}{}", hexdb_core::replication::peer_base_url(&overseer), path_and_query);
    let mut headers = HeaderMap::new();
    copy_headers(&parts.headers, &mut headers);
    let signature = engine.lattice_keys.sign_request(parts.method.as_str(), &signed_target(&path_and_query, &client), b"");
    if let (Ok(c), Ok(s)) = (HeaderValue::from_str(&client), HeaderValue::from_str(&signature)) {
        headers.insert(HeaderName::from_static(FORWARDED_FOR_HEADER), c);
        headers.insert(HeaderName::from_static(FORWARD_SIGNATURE_HEADER), s);
    }
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).unwrap_or(reqwest::Method::PUT);
    debug!("↪️ Streaming {} {} to the Overseer '{}'.", parts.method, parts.uri.path(), overseer.name);
    let streamed = reqwest::Body::wrap_stream(body.into_data_stream());
    let response = match http.request(method, &url).timeout(Duration::from_secs(24 * 3600)).headers(headers).body(streamed).send().await {
        Ok(response) => response,
        Err(e) => {
            warn!("⚠️ Forwarding an upload to the Overseer failed: {}", e);
            return crate::handlers::ApiError::new(StatusCode::BAD_GATEWAY, "overseer_unreachable", format!("The upload couldn't be passed on to the Overseer '{}': {}", overseer.name, e)).into_response();
        }
    };
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out_headers = HeaderMap::new();
    copy_headers(response.headers(), &mut out_headers);
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(e) => return crate::handlers::ApiError::new(StatusCode::BAD_GATEWAY, "overseer_unreachable", format!("The Overseer's response was cut off: {}", e)).into_response(),
    };
    let mut out = Response::new(Body::from(body));
    *out.status_mut() = status;
    *out.headers_mut() = out_headers;
    if let Ok(name) = HeaderValue::from_str(&overseer.name) {
        out.headers_mut().insert(HeaderName::from_static(FORWARDED_TO_HEADER), name);
    }
    out.headers_mut().remove(header::CONTENT_LENGTH);
    out
}

/// Forward writes to the Overseer when this hex is a replica.
pub async fn forward_writes(State(engine): State<Arc<HexDBEngine>>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    if engine.is_writable() || !engine.config.replication.forward_writes || handled_locally(request.method(), &path) {
        return next.run(request).await;
    }
    let overseer = {
        let peers = engine.peers.lock().await;
        peers.iter().find(|p| p.status == "active" && p.hex.role == ROLE_OVERSEER).map(|p| p.hex.clone())
    };
    let Some(overseer) = overseer else { return next.run(request).await };
    if path.contains("/_files/") || (request.method() == Method::PATCH && path.contains("/_uploads/")) {
        return forward_upload(&engine, overseer, request).await;
    }

    let (parts, body) = request.into_parts();
    let bytes: Bytes = match axum::body::to_bytes(body, crate::routes::bulk_body_limit(&engine)).await {
        Ok(bytes) => bytes,
        Err(_) => return crate::handlers::ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "The request body is too large.").into_response(),
    };
    if path == "/graphql" && !graphql_mutation(&bytes) {
        return next.run(Request::from_parts(parts, Body::from(bytes))).await;
    }

    let client = crate::auth::client_address(&parts.extensions);
    let path_and_query = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or(path.clone());
    let url = format!("{}{}", hexdb_core::replication::peer_base_url(&overseer), path_and_query);
    let Some(http) = forward_client(&engine) else {
        return next.run(Request::from_parts(parts, Body::from(bytes))).await;
    };
    let timeout = Duration::from_secs(engine.config.limits.request_timeout_seconds.max(1));
    let mut headers = HeaderMap::new();
    copy_headers(&parts.headers, &mut headers);
    let signature = engine.lattice_keys.sign_request(parts.method.as_str(), &signed_target(&path_and_query, &client), b"");
    if let (Ok(c), Ok(s)) = (HeaderValue::from_str(&client), HeaderValue::from_str(&signature)) {
        headers.insert(HeaderName::from_static(FORWARDED_FOR_HEADER), c);
        headers.insert(HeaderName::from_static(FORWARD_SIGNATURE_HEADER), s);
    }
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).unwrap_or(reqwest::Method::POST);
    debug!("↪️ Forwarding {} {} to the Overseer '{}'.", parts.method, path, overseer.name);
    let response = match http.request(method, &url).timeout(timeout).headers(headers).body(bytes.clone()).send().await {
        Ok(response) => response,
        Err(e) => {
            warn!("⚠️ Forwarding {} {} to the Overseer failed: {}", parts.method, path, e);
            return crate::handlers::ApiError::new(
                StatusCode::BAD_GATEWAY,
                "overseer_unreachable",
                format!("This hex is a replica and couldn't reach the Overseer '{}' to make the change. Try again shortly.", overseer.name),
            )
            .into_response();
        }
    };
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out_headers = HeaderMap::new();
    copy_headers(response.headers(), &mut out_headers);
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(e) => {
            return crate::handlers::ApiError::new(StatusCode::BAD_GATEWAY, "overseer_unreachable", format!("The Overseer's response was cut off: {}", e)).into_response();
        }
    };
    let mut out = Response::new(Body::from(body));
    *out.status_mut() = status;
    *out.headers_mut() = out_headers;
    if let Ok(name) = HeaderValue::from_str(&overseer.name) {
        out.headers_mut().insert(HeaderName::from_static(FORWARDED_TO_HEADER), name);
    }
    out.headers_mut().remove(header::CONTENT_LENGTH);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_hex_requests_stay_on_this_hex() {
        for path in ["/settings", "/backup", "/scrub", "/flush", "/compact", "/shutdown", "/analyzers/_analyze", "/orders/_query"] {
            assert!(handled_locally(&Method::POST, path) || handled_locally(&Method::PUT, path), "{}", path);
        }
        assert!(handled_locally(&Method::PUT, "/settings"), "runtime settings belong to the hex they're sent to");
        for (method, path) in [(Method::POST, "/orders"), (Method::PUT, "/config"), (Method::DELETE, "/orders/01J0"), (Method::POST, "/transactions")] {
            assert!(!handled_locally(&method, path), "{} {}", method, path);
        }
    }
}
