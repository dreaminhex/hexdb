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
// claim another address by sending the header itself. With
// `partitioning.enabled` (once every hex runs 2.6 or later), the signature also
// covers the epoch of the partition map the request was routed by; a hex whose
// map has another epoch refuses it untouched (421 `partition_epoch`), and the
// forwarding hex reloads its map and sends it again (see crate::partitioning
// in hexdb_core). Reads, sign-in and
// sign-out, this hex's own maintenance and settings (flush, compact, scrub,
// backup, shutdown, `PUT /settings`), analyzer previews and hex-to-hex
// endpoints are always handled locally. GraphQL requests are forwarded only when they
// contain a mutation.
//
// File uploads (`/_files/`, and parts of resumable uploads) are streamed to the Overseer as they arrive, with
// no size limit or timeout here; other bodies are read first (up to
// `limits.max_request_mb`).
//
// While there's no Overseer (an election), or while this hex hands its role
// over, a write waits up to `HOLD_FOR_OVERSEER` for one instead of failing,
// and one whose connection to the Overseer was refused is tried again then (a
// refused connection never reached it, so it can't run twice). A read, or a
// write with an idempotency key, is also sent again after a connection that
// broke mid-request: running either twice changes nothing. Without an
// Overseer after that, the request is handled locally, which refuses writes
// with 421 `read_only_replica`.
//
// A sliced replica (`replication.slicing`) keeps only some of the data, so
// it forwards reads of user data too. Server-Sent Events feeds stream through
// from the Overseer like any other answer; WebSocket feeds can't be passed on,
// so those are refused with 421 naming the Overseer. While a newly elected
// Overseer collects slices, it answers data requests with 503.

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use hexdb_core::{
    engine::HexDBEngine,
    network::discovery::ROLE_OVERSEER,
    partitioning::{EPOCH_MISMATCH_CODE, PARTITION_EPOCH_HEADER},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::{debug, warn};

/// How long a write waits for an Overseer (an election, a handover) before it's refused.
const HOLD_FOR_OVERSEER: Duration = Duration::from_secs(30);

/// The Overseer to pass a request to, waiting until `until` while there's none.
/// A hex handing its role over waits for another hex to hold it.
async fn overseer_until(engine: &HexDBEngine, until: Instant) -> Option<hexdb_core::network::discovery::PeerHex> {
    loop {
        let found = engine.peers.lock().await.iter().find(|p| p.status == "active" && p.hex.role == ROLE_OVERSEER).map(|p| p.hex.clone());
        if found.is_some() || Instant::now() >= until {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The client address a replica forwarded a request for.
pub const FORWARDED_FOR_HEADER: &str = "x-hexdb-forwarded-for";
/// Lattice signature over the forwarded request's method, path and client address.
pub const FORWARD_SIGNATURE_HEADER: &str = "x-hexdb-forward-signature";
/// Response header naming the Overseer that handled a forwarded request.
pub const FORWARDED_TO_HEADER: &str = "x-hexdb-forwarded-to";

/// Marks a request a replica forwarded (verified by its signature).
#[derive(Clone, Copy)]
pub struct Forwarded;

/// The verified signature of a forwarded request, so its JSON answer can be
/// signed for it (see `crate::lattice::sign_responses`).
#[derive(Clone)]
pub struct ForwardSignature(pub String);

/// Answers larger than this aren't buffered to be signed or checked.
pub const MAX_SIGNED_ANSWER: u64 = 64 * 1024 * 1024;

/// True for an answer the Overseer signs and a replica checks: JSON within the size limit.
pub fn signed_answer(headers: &HeaderMap) -> bool {
    let json = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("application/json"));
    let small = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()).is_none_or(|n| n <= MAX_SIGNED_ANSWER);
    json && small
}

/// Read the Overseer's answer and check its signature, when the Overseer signs
/// forwarded answers and this one is JSON. Err: a ready 502 to return.
enum Answer {
    /// Read whole, and its signature checked.
    Checked(axum::body::Bytes),
    /// Passed through as it comes (not JSON, or the Overseer doesn't sign).
    Unchecked(reqwest::Response),
}

async fn checked_answer(engine: &HexDBEngine, overseer: &hexdb_core::network::discovery::PeerHex, signature: &str, response: reqwest::Response, incoming: Option<&str>) -> Result<(StatusCode, HeaderMap, Answer), crate::handlers::ApiError> {
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out_headers = HeaderMap::new();
    copy_headers(response.headers(), &mut out_headers);
    let json = signed_answer(response.headers());
    if !json || (!overseer.signs_forwarded && incoming.is_none()) {
        return Ok((status, out_headers, Answer::Unchecked(response)));
    }
    let tag = response.headers().get(hexdb_core::replication::LATTICE_RESPONSE_HEADER).and_then(|v| v.to_str().ok()).map(String::from);
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(e) => return Err(crate::handlers::ApiError::new(StatusCode::BAD_GATEWAY, "overseer_unreachable", format!("The Overseer's response was cut off: {}", e))),
    };
    if overseer.signs_forwarded && !tag.is_some_and(|tag| engine.lattice_keys.verify_response(signature, status.as_u16(), &body, &tag)) {
        warn!("🚫 The Overseer '{}' sent an answer to a forwarded request without a valid signature; not passing it on.", overseer.name);
        return Err(crate::handlers::ApiError::new(StatusCode::BAD_GATEWAY, "overseer_unreachable", "The Overseer's answer didn't verify (it may have been changed in transit); the request may have been carried out."));
    }
    out_headers.remove(hexdb_core::replication::LATTICE_RESPONSE_HEADER);
    // This hex was passing on a request forwarded to it (an Overseer handing its
    // role over): sign the answer for the hex that sent it here.
    if let Some(incoming) = incoming {
        if let Ok(tag) = HeaderValue::from_str(&engine.lattice_keys.sign_response(incoming, status.as_u16(), &body)) {
            out_headers.insert(HeaderName::from_static(hexdb_core::replication::LATTICE_RESPONSE_HEADER), tag);
        }
    }
    Ok((status, out_headers, Answer::Checked(body)))
}

/// What the forwarding signature covers. The partition epoch only when one is sent,
/// so hexes before 2.6 (which never get one) check the same text as before.
pub fn signed_target(path_and_query: &str, client: &str, epoch: Option<u64>) -> String {
    match epoch {
        Some(epoch) => format!("{}#forwarded-for={}#partition-epoch={}", path_and_query, client, epoch),
        None => format!("{}#forwarded-for={}", path_and_query, client),
    }
}

/// The partition epoch a forwarded request was routed by (set when its signature checked out).
#[derive(Clone, Copy, Debug)]
pub struct PartitionEpoch(pub u64);

/// Signs a forwarded request into `headers`: the client address, the signature and,
/// when this hex sends them, the partition epoch. Returns the signature.
async fn sign_forward(engine: &HexDBEngine, method: &str, path_and_query: &str, client: &str, headers: &mut HeaderMap) -> String {
    let epoch = if engine.sends_partition_epochs().await { Some(engine.partition_map().epoch) } else { None };
    let signature = engine.lattice_keys.sign_request(method, &signed_target(path_and_query, client, epoch), b"");
    if let (Ok(c), Ok(s)) = (HeaderValue::from_str(client), HeaderValue::from_str(&signature)) {
        headers.insert(HeaderName::from_static(FORWARDED_FOR_HEADER), c);
        headers.insert(HeaderName::from_static(FORWARD_SIGNATURE_HEADER), s);
    }
    match epoch {
        Some(epoch) => {
            headers.insert(HeaderName::from_static(PARTITION_EPOCH_HEADER), HeaderValue::from(epoch));
        }
        None => {
            headers.remove(PARTITION_EPOCH_HEADER);
        }
    }
    signature
}

/// A forwarded request routed by another partition map than this hex's: refused before
/// anything runs, so the sender can reload its map and send it again. A hex that may be
/// behind reads its map first.
async fn epoch_mismatch(engine: &HexDBEngine, sent: Option<PartitionEpoch>) -> Option<Response> {
    let PartitionEpoch(theirs) = sent?;
    if engine.partition_map().epoch != theirs {
        engine.refresh_partition_map().await;
    }
    let ours = engine.partition_map().epoch;
    (ours != theirs).then(|| {
        crate::handlers::ApiError::new(
            StatusCode::MISDIRECTED_REQUEST,
            EPOCH_MISMATCH_CODE,
            format!("This request was routed by partition map epoch {}; this hex is at epoch {}. Send it again.", theirs, ours),
        )
        .into_response()
    })
}

/// True if an answer is the refusal above.
fn is_epoch_mismatch(status: StatusCode, body: &[u8]) -> bool {
    status == StatusCode::MISDIRECTED_REQUEST && serde_json::from_slice::<serde_json::Value>(body).ok().is_some_and(|v| v["error"]["code"] == EPOCH_MISMATCH_CODE)
}

fn handled_locally(method: &Method, path: &str) -> bool {
    // A rolling update runs on the Overseer, so it's the one that can say how it's going.
    // Quarantine lives on the Overseer (it scans every upload), so reads of it go there too.
    (matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) && path != "/updates" && !path.starts_with("/quarantine"))
        || path.starts_with("/lattice/")
        || crate::routes::is_ui_path(path)
        || matches!(path, "/auth/login" | "/auth/logout" | "/shutdown" | "/flush" | "/compact" | "/scrub" | "/backup" | "/settings" | "/analyzers/_analyze")
        || path.ends_with("/_query")
        || path.ends_with("/_aggregate")
        || path == "/sql"
        // MCP only reads; any hex answers.
        || path == "/mcp"
        // Sharing links only read (download counts go to the Overseer themselves).
        || path.starts_with("/_s/")
}

/// What a sliced replica (or a rebuilding Overseer) still answers itself:
/// requests that don't read user data.
fn answers_without_data(path: &str) -> bool {
    path.starts_with("/lattice/")
        || crate::routes::is_ui_path(path)
        || matches!(
            path,
            "/" | "/health"
                | "/ready"
                | "/status"
                | "/status/history"
                | "/logs"
                | "/openapi.json"
                | "/auth/login"
                | "/auth/logout"
                | "/auth/me"
                | "/settings"
                | "/partitions"
                | "/shutdown"
                | "/flush"
                | "/compact"
                | "/scrub"
                | "/backup"
                | "/backups"
                | "/join"
        )
}

/// Long-lived feeds, which forwarding can't carry.
/// A WebSocket feed (`/changes/ws`, `/streams/{name}/ws`): an upgraded connection,
/// which forwarding can't pass on. Server-Sent Events feeds stream through.
fn websocket_feed(path: &str) -> bool {
    path.ends_with("/ws")
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
        if HOP_BY_HOP.contains(&n) || n == FORWARDED_FOR_HEADER || n == FORWARD_SIGNATURE_HEADER || n == PARTITION_EPOCH_HEADER {
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
    let incoming = parts.extensions.get::<ForwardSignature>().map(|s| s.0.clone());
    let client = crate::auth::client_address(&parts.extensions);
    let path_and_query = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| parts.uri.path().to_string());
    let url = format!("{}{}", hexdb_core::replication::peer_base_url(&overseer), path_and_query);
    let mut headers = HeaderMap::new();
    copy_headers(&parts.headers, &mut headers);
    // A streamed upload can't be sent twice: a `partition_epoch` refusal goes back to the client,
    // which can send it again (nothing was written).
    let signature = sign_forward(engine, parts.method.as_str(), &path_and_query, &client, &mut headers).await;
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
    let (status, out_headers, answer) = match checked_answer(engine, &overseer, &signature, response, incoming.as_deref()).await {
        Ok(answer) => answer,
        Err(refused) => return refused.into_response(),
    };
    let body = match answer {
        Answer::Checked(body) => body,
        Answer::Unchecked(response) => match response.bytes().await {
            Ok(body) => body,
            Err(e) => return crate::handlers::ApiError::new(StatusCode::BAD_GATEWAY, "overseer_unreachable", format!("The Overseer's response was cut off: {}", e)).into_response(),
        },
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

/// Pass an AI question to the Oracle and its answer back (streamed: A2A answers
/// with server-sent events). Err: the Oracle couldn't be reached, so the request
/// is handed back to be answered here.
async fn forward_to_oracle(engine: &Arc<HexDBEngine>, oracle: &hexdb_core::network::discovery::PeerHex, request: Request) -> Result<Response, Box<Request>> {
    let Some(http) = forward_client(engine) else { return Err(Box::new(request)) };
    let (parts, body) = request.into_parts();
    let bytes: Bytes = match axum::body::to_bytes(body, crate::routes::bulk_body_limit(engine)).await {
        Ok(bytes) => bytes,
        Err(_) => return Ok(crate::handlers::ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "The request body is too large.").into_response()),
    };
    let client = crate::auth::client_address(&parts.extensions);
    let path_and_query = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| parts.uri.path().to_string());
    let mut headers = HeaderMap::new();
    copy_headers(&parts.headers, &mut headers);
    let signature = engine.lattice_keys.sign_request(parts.method.as_str(), &signed_target(&path_and_query, &client, None), b"");
    if let (Ok(c), Ok(s)) = (HeaderValue::from_str(&client), HeaderValue::from_str(&signature)) {
        headers.insert(HeaderName::from_static(FORWARDED_FOR_HEADER), c);
        headers.insert(HeaderName::from_static(FORWARD_SIGNATURE_HEADER), s);
    }
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).unwrap_or(reqwest::Method::POST);
    let timeout = if crate::server::is_long_running(parts.uri.path()) { Duration::from_secs(3600) } else { Duration::from_secs(engine.config.limits.request_timeout_seconds.max(1)) };
    let url = format!("{}{}", hexdb_core::replication::peer_base_url(oracle), path_and_query);
    debug!("🔮 Passing {} {} to the Oracle '{}'.", parts.method, parts.uri.path(), oracle.name);
    let response = match http.request(method, &url).timeout(timeout).headers(headers).body(bytes.clone()).send().await {
        Ok(response) => response,
        // Never reached the Oracle: answering here is safe. Anything else may have run there.
        Err(e) if e.is_connect() => {
            warn!("⚠️ The Oracle '{}' couldn't be reached ({}); answering here.", oracle.name, e);
            return Err(Box::new(Request::from_parts(parts, Body::from(bytes))));
        }
        Err(e) => {
            return Ok(crate::handlers::ApiError::new(StatusCode::BAD_GATEWAY, "oracle_unreachable", format!("The Oracle '{}' didn't answer: {}", oracle.name, e)).into_response());
        }
    };
    // The Oracle doesn't know a caller this hex just authenticated (a new user or
    // key it hasn't replicated yet): answer here this time.
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        debug!("🔮 The Oracle '{}' doesn't know this caller yet; answering here.", oracle.name);
        return Err(Box::new(Request::from_parts(parts, Body::from(bytes))));
    }
    let (status, out_headers, answer) = match checked_answer(engine, oracle, &signature, response, None).await {
        Ok(answer) => answer,
        Err(refused) => return Ok(refused.into_response()),
    };
    let body = match answer {
        Answer::Checked(body) => Body::from(body),
        Answer::Unchecked(response) => Body::from_stream(response.bytes_stream()),
    };
    let mut out = Response::new(body);
    *out.status_mut() = status;
    *out.headers_mut() = out_headers;
    if let Ok(name) = HeaderValue::from_str(&oracle.name) {
        out.headers_mut().insert(HeaderName::from_static(FORWARDED_TO_HEADER), name);
    }
    out.headers_mut().remove(header::CONTENT_LENGTH);
    Ok(out)
}

/// Forward writes to the Overseer when this hex is a replica.
pub async fn forward_writes(State(engine): State<Arc<HexDBEngine>>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let sent = request.extensions().get::<PartitionEpoch>().copied();
    if let Some(refused) = epoch_mismatch(&engine, sent).await {
        return refused;
    }
    // AI questions go to the lattice's Oracle while one is live; the Oracle
    // answers them itself (see crate::oracle). One already passed on isn't passed again.
    if crate::oracle::is_ai_question(request.method(), &path) {
        if engine.is_oracle() {
            return next.run(request).await;
        }
        if request.extensions().get::<ForwardSignature>().is_none() {
            if let Some(oracle) = crate::oracle::live_oracle(&engine).await {
                return match forward_to_oracle(&engine, &oracle, request).await {
                    Ok(response) => response,
                    // It couldn't be reached: answer here, as without an Oracle.
                    Err(request) => next.run(*request).await,
                };
            }
        }
    }
    if let Some(note) = engine.rebuilding() {
        if !answers_without_data(&path) {
            return crate::handlers::ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "rebuilding", note).with_retry_after(5).into_response();
        }
    }
    // A sliced replica only has some of the data: everything that reads it goes to the Overseer.
    let partial = !engine.is_writable() && engine.is_partial_replica() && !answers_without_data(&path);
    // An Overseer handing its role over passes writes on to the next one.
    let writing_here = engine.is_writable() && !hexdb_core::updates::stepping_down();
    let mut request = request;
    if !partial && (writing_here || !engine.config.replication.forward_writes || handled_locally(request.method(), &path)) {
        let resendable = writing_here
            && engine.config.replication.forward_writes
            && !handled_locally(request.method(), &path)
            && !path.contains("/_files/")
            && !path.contains("/_uploads/")
            && !crate::server::is_long_running(&path);
        if !resendable {
            return next.run(request).await;
        }
        // A write that arrived just before this Overseer began handing its role over is refused
        // untouched (`stepping_down`): it goes on to the next Overseer, as a replica would send it,
        // instead of failing. So the body is kept until the answer is known.
        let (parts, body) = request.into_parts();
        let bytes: Bytes = match axum::body::to_bytes(body, crate::routes::bulk_body_limit(&engine)).await {
            Ok(bytes) => bytes,
            Err(_) => return crate::handlers::ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "The request body is too large.").into_response(),
        };
        let response = next.clone().run(Request::from_parts(parts.clone(), Body::from(bytes.clone()))).await;
        if response.status() != StatusCode::SERVICE_UNAVAILABLE {
            return response;
        }
        let (response_parts, response_body) = response.into_parts();
        let answer = axum::body::to_bytes(response_body, 64 * 1024).await.unwrap_or_default();
        let refused_untouched = serde_json::from_slice::<serde_json::Value>(&answer).ok().is_some_and(|v| v["error"]["code"] == crate::handlers::STEPPING_DOWN_CODE);
        if !refused_untouched {
            return Response::from_parts(response_parts, Body::from(answer));
        }
        debug!("A write reached this Overseer as it began handing its role over; sending it to the next one.");
        request = Request::from_parts(parts, Body::from(bytes));
    }
    let until = Instant::now() + HOLD_FOR_OVERSEER;
    let overseer = overseer_until(&engine, until).await;
    let Some(overseer) = overseer else {
        if partial {
            return crate::handlers::ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "overseer_unreachable",
                "This hex keeps only some of the lattice's slices and no Overseer is reachable to answer; try again shortly.",
            )
            .with_retry_after(5)
            .into_response();
        }
        return next.run(request).await;
    };
    if partial && websocket_feed(&path) {
        return crate::handlers::ApiError::new(
            StatusCode::MISDIRECTED_REQUEST,
            "sliced_replica",
            format!(
                "This hex keeps only some of the lattice's slices, so it doesn't serve WebSocket feeds. Connect to the Overseer '{}' at {}, or use the Server-Sent Events feed, which any hex passes on.",
                overseer.name,
                hexdb_core::replication::peer_base_url(&overseer)
            ),
        )
        .into_response();
    }
    if path.contains("/_files/") || (request.method() == Method::PATCH && path.contains("/_uploads/")) {
        return forward_upload(&engine, overseer, request).await;
    }

    let (parts, body) = request.into_parts();
    let incoming = parts.extensions.get::<ForwardSignature>().map(|s| s.0.clone());
    let bytes: Bytes = match axum::body::to_bytes(body, crate::routes::bulk_body_limit(&engine)).await {
        Ok(bytes) => bytes,
        Err(_) => return crate::handlers::ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "The request body is too large.").into_response(),
    };
    if !partial && path == "/graphql" && !graphql_mutation(&bytes) {
        return next.run(Request::from_parts(parts, Body::from(bytes))).await;
    }

    let client = crate::auth::client_address(&parts.extensions);
    let path_and_query = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or(path.clone());
    let Some(http) = forward_client(&engine) else {
        return next.run(Request::from_parts(parts, Body::from(bytes))).await;
    };
    // Long-running routes (A2A streams) keep their own limits; forwarding mustn't cut them off.
    let timeout = if crate::server::is_long_running(&path) { Duration::from_secs(3600) } else { Duration::from_secs(engine.config.limits.request_timeout_seconds.max(1)) };
    let mut headers = HeaderMap::new();
    copy_headers(&parts.headers, &mut headers);
    let mut signature = sign_forward(&engine, parts.method.as_str(), &path_and_query, &client, &mut headers).await;
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).unwrap_or(reqwest::Method::POST);
    // A write the Overseer makes idempotent carries a key (the client's, or one added
    // here), so it can be sent again after an error that leaves open whether the
    // Overseer got it: the Overseer replays the first answer instead of writing twice.
    let resendable = idempotent_with_key(&engine, &parts.method, &path);
    if resendable && !headers.contains_key(crate::handlers::IDEMPOTENCY_KEY_HEADER) {
        if let Ok(key) = HeaderValue::from_str(&format!("{}{}", hexdb_core::engine::FORWARDED_KEY_PREFIX, ulid::Ulid::generate())) {
            headers.insert(HeaderName::from_static(crate::handlers::IDEMPOTENCY_KEY_HEADER), key);
        }
    }
    // A read (a sliced replica forwards those too) changes nothing, so it's always safe to send again.
    let read = matches!(parts.method, Method::GET | Method::HEAD);
    let mut overseer = overseer;
    // Refusals for a stale partition map, answered so far (each is sent again with the reloaded map).
    let mut epoch_refusals = 0;
    let (status, out_headers, answer) = loop {
    let response = loop {
        debug!("↪️ Forwarding {} {} to the Overseer '{}'.", parts.method, path, overseer.name);
        let url = format!("{}{}", hexdb_core::replication::peer_base_url(&overseer), path_and_query);
        match http.request(method.clone(), &url).timeout(timeout).headers(headers.clone()).body(bytes.clone()).send().await {
            Ok(response) => break response,
            Err(e) => {
                // A refused connection never reached the Overseer (it's restarting, or
                // gone): try the Overseer the lattice has once one is there. Other send
                // errors (a connection the stopping Overseer closed) may have reached it,
                // so only a read or a keyed write is sent again. A timeout isn't: it may still be running.
                if (e.is_connect() || ((resendable || read) && !e.is_timeout())) && Instant::now() < until {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    if let Some(next) = overseer_until(&engine, until).await {
                        overseer = next;
                        continue;
                    }
                }
                warn!("⚠️ Forwarding {} {} to the Overseer failed: {}", parts.method, path, e);
                return crate::handlers::ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "overseer_unreachable",
                    format!("This hex is a replica and couldn't reach the Overseer '{}' to make the change. Try again shortly.", overseer.name),
                )
                .into_response();
            }
        }
    };
    let (status, out_headers, answer) = match checked_answer(&engine, &overseer, &signature, response, incoming.as_deref()).await {
        Ok(answer) => answer,
        Err(refused) => return refused.into_response(),
    };
    if status != StatusCode::MISDIRECTED_REQUEST || epoch_refusals >= 3 {
        break (status, out_headers, answer);
    }
    let body = match answer {
        Answer::Checked(body) => body,
        Answer::Unchecked(response) => response.bytes().await.unwrap_or_default(),
    };
    if !is_epoch_mismatch(status, &body) {
        break (status, out_headers, Answer::Checked(body));
    }
    // Refused untouched for a stale partition map: reload it and send again, a few times at most.
    epoch_refusals += 1;
    debug!("The Overseer '{}' routes by another partition map epoch; reloading this hex's and sending {} {} again.", overseer.name, parts.method, path);
    engine.refresh_partition_map().await;
    signature = sign_forward(&engine, parts.method.as_str(), &path_and_query, &client, &mut headers).await;
    tokio::time::sleep(Duration::from_millis(100 * epoch_refusals)).await;
    };
    // Other answers stream through, so a forwarded download isn't held in memory.
    let body = match answer {
        Answer::Checked(body) => Body::from(body),
        Answer::Unchecked(response) => Body::from_stream(response.bytes_stream()),
    };
    let mut out = Response::new(body);
    *out.status_mut() = status;
    *out.headers_mut() = out_headers;
    if let Ok(name) = HeaderValue::from_str(&overseer.name) {
        out.headers_mut().insert(HeaderName::from_static(FORWARDED_TO_HEADER), name);
    }
    out.headers_mut().remove(header::CONTENT_LENGTH);
    out
}

/// Writes the Overseer makes idempotent with an `Idempotency-Key` header: documents
/// in user tessellations (single, bulk, `_update`) and transactions.
fn idempotent_with_key(engine: &HexDBEngine, method: &Method, path: &str) -> bool {
    if !matches!(*method, Method::POST | Method::PUT | Method::PATCH | Method::DELETE) {
        return false;
    }
    if path == "/transactions" {
        return true;
    }
    let first = path.trim_start_matches('/').split('/').next().unwrap_or_default();
    engine.tessellation_info(first).is_some_and(|t| t.kind == "user")
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

    #[test]
    fn the_signature_covers_the_epoch_only_when_one_is_sent() {
        // Hexes before 2.6 check exactly this text.
        assert_eq!(signed_target("/orders?x=1", "10.0.0.1", None), "/orders?x=1#forwarded-for=10.0.0.1");
        assert_ne!(signed_target("/orders", "10.0.0.1", Some(1)), signed_target("/orders", "10.0.0.1", Some(2)));
        assert_ne!(signed_target("/orders", "10.0.0.1", Some(1)), signed_target("/orders", "10.0.0.1", None));
        let refusal = serde_json::json!({ "error": { "code": EPOCH_MISMATCH_CODE, "message": "" } }).to_string();
        assert!(is_epoch_mismatch(StatusCode::MISDIRECTED_REQUEST, refusal.as_bytes()));
        assert!(!is_epoch_mismatch(StatusCode::MISDIRECTED_REQUEST, br#"{"error":{"code":"sliced_replica"}}"#));
        assert!(!is_epoch_mismatch(StatusCode::CONFLICT, refusal.as_bytes()));
    }
}
