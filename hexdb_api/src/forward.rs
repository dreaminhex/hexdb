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
// sign-out, maintenance (flush, compact, shutdown) and hex-to-hex endpoints
// are always handled locally. GraphQL requests are forwarded only when they
// contain a mutation.
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
        || matches!(path, "/auth/login" | "/auth/logout" | "/shutdown" | "/flush" | "/compact")
        || path.ends_with("/_query")
        || path.ends_with("/_aggregate")
        || path == "/sql"
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
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    let client_or_none = CLIENT.get_or_init(|| match hexdb_core::replication::lattice_client(&engine.config, Duration::from_secs(300)) {
        Ok(client) => Some(client),
        Err(e) => {
            warn!("⚠️ Can't forward writes to the Overseer: {:#}", e);
            None
        }
    });
    let Some(http) = client_or_none.clone() else {
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
