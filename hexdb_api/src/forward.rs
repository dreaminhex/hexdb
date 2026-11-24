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

/// The Overseer of `partition` (or, with None, of this hex's own group), waiting until
/// `until` while there's none.
async fn overseer_for(engine: &HexDBEngine, until: Instant, partition: Option<u32>) -> Option<hexdb_core::network::discovery::PeerHex> {
    let Some(partition) = partition else { return overseer_until(engine, until).await };
    loop {
        let map = engine.partition_map();
        let (found, members) = {
            let peers = engine.peers.lock().await;
            let in_partition: Vec<_> = peers.iter().filter(|p| p.status == "active" && map.partition_of_hex(&p.hex.id) == Some(partition)).collect();
            (in_partition.iter().find(|p| p.hex.role == ROLE_OVERSEER).map(|p| p.hex.clone()), in_partition.len())
        };
        // No live hex of that partition at all: nothing to wait for (none is about to be elected).
        if found.is_some() || members == 0 || Instant::now() >= until {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// API routes that aren't lattice-wide system writes: they run on the hex that gets them
/// (transactions, the delete history and trash, which stay in their partition, GraphQL,
/// agents, hex-specific operations). Every other route (users, roles, keys, the
/// configuration, tessellation definitions, functions, streams...) is partition 0's.
const NOT_PARTITION_ZERO: &[&str] = &["transactions", "graphql", "a2a", "deletes", "mcp", "sql", "_s", "lattice", "updates", "quarantine", "health", "ready", "status", "logs", "changes", "backup", "backups", "flush", "compact", "scrub", "shutdown", "settings", "retire"];

/// On a partitioned lattice, the partition a write must run in when that isn't
/// necessarily this hex's: a change to an existing document (by its ID's slice), or a
/// lattice-wide system write (users, keys, the configuration, tessellation definitions),
/// which is partition 0's. None: run it here (reads, inserts, which draw an ID in this
/// hex's partition, bulk writes and anything hex-specific; the commit refuses a
/// document that isn't this partition's).
fn routed_partition(engine: &HexDBEngine, method: &Method, path: &str) -> Option<u32> {
    let map = engine.partition_map();
    if map.is_single() {
        return None;
    }
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let first = segments.first().copied().unwrap_or_default();
    // One document's read goes to the partition that holds it.
    if matches!(*method, Method::GET | Method::HEAD) && segments.len() == 2 && !hexdb_core::catalog::is_route_name(first) && !first.starts_with('_') {
        return ulid::Ulid::from_string(segments[1]).ok().map(|id| map.partition_of(first, id));
    }
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) || handled_locally(method, path) {
        return None;
    }
    // Signing in reads users this hex has as copies; everything else under /auth changes them.
    if first == "auth" {
        return if matches!(segments.get(1).copied(), Some("login") | Some("me")) { None } else { Some(0) };
    }
    // Not an API route: a user tessellation, even one this hex hasn't heard of yet (just created elsewhere).
    if !hexdb_core::catalog::is_route_name(first) {
        if segments.len() == 2 && matches!(*method, Method::PUT | Method::PATCH | Method::DELETE) {
            if let Ok(id) = ulid::Ulid::from_string(segments[1]) {
                return Some(map.partition_of(first, id));
            }
        }
        return None;
    }
    if NOT_PARTITION_ZERO.contains(&first) || first.is_empty() {
        return None;
    }
    Some(0)
}

/// The Overseer to pass a request to, waiting until `until` while there's none.
/// A hex handing its role over waits for another hex to hold it.
async fn overseer_until(engine: &HexDBEngine, until: Instant) -> Option<hexdb_core::network::discovery::PeerHex> {
    loop {
        // This hex's partition group's Overseer (writes for other partitions are routed in stage 2's step 5).
        let found = engine.peers.lock().await.iter().find(|p| p.status == "active" && p.hex.role == ROLE_OVERSEER && engine.in_my_group(&p.hex.id)).map(|p| p.hex.clone());
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

/// A `POST /backup` of the whole lattice (the default scope) that this hex can't
/// take, being a replica without all of it: the Overseer takes it instead.
fn lattice_backup_elsewhere(engine: &HexDBEngine, request: &Request) -> bool {
    if request.method() != Method::POST || request.uri().path() != "/backup" || engine.is_writable() {
        return false;
    }
    let scope = axum::extract::Query::<crate::handlers::BackupQuery>::try_from_uri(request.uri()).ok().and_then(|q| q.0.scope);
    matches!(crate::handlers::backup_scope(scope.as_deref()), Ok(hexdb_core::engine::BackupScope::Lattice)) && !engine.backup_coverage().1
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
                | "/status/partitions"
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
    respond(status, out_headers, answer, &overseer)
}

/// The Overseer's answer for the client. One that wasn't checked (a download, say)
/// streams through as it comes, with its length, so it's never held in memory here.
fn respond(status: StatusCode, headers: HeaderMap, answer: Answer, overseer: &hexdb_core::network::discovery::PeerHex) -> Response {
    let (body, length) = match answer {
        Answer::Checked(body) => (Body::from(body), None),
        Answer::Unchecked(response) => {
            let length = response.headers().get(header::CONTENT_LENGTH).cloned();
            (Body::from_stream(response.bytes_stream()), length)
        }
    };
    let mut out = Response::new(body);
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    if let Ok(name) = HeaderValue::from_str(&overseer.name) {
        out.headers_mut().insert(HeaderName::from_static(FORWARDED_TO_HEADER), name);
    }
    match length {
        Some(length) => out.headers_mut().insert(header::CONTENT_LENGTH, length),
        None => out.headers_mut().remove(header::CONTENT_LENGTH),
    };
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
    // A partitioned lattice answers queries and counts from every partition.
    let request = match fan_out_read(&engine, request, &next).await {
        Ok(response) => return response,
        Err(request) => *request,
    };
    if let Some(note) = engine.rebuilding() {
        if !answers_without_data(&path) {
            return crate::handlers::ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "rebuilding", note).with_retry_after(5).into_response();
        }
    }
    // A sliced replica only has some of the data: everything that reads it goes to the Overseer.
    // So does a lattice backup sent to a replica that doesn't hold the whole lattice.
    let partial = (!engine.is_writable() && engine.is_partial_replica() && !answers_without_data(&path)) || lattice_backup_elsewhere(&engine, &request);
    // A write that another partition must run goes to that partition's Overseer.
    let elsewhere = routed_partition(&engine, request.method(), &path).filter(|p| Some(*p) != engine.my_partition());
    // An Overseer handing its role over passes writes on to the next one.
    let writing_here = engine.is_writable() && !hexdb_core::updates::stepping_down();
    let mut request = request;
    if elsewhere.is_none() && !partial && (writing_here || !engine.config.replication.forward_writes || handled_locally(request.method(), &path)) {
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
    let overseer = overseer_for(&engine, until, elsewhere).await;
    // Another partition's write with no Overseer of it reachable: refused untouched, never run here.
    if let (None, Some(partition)) = (&overseer, elsewhere) {
        return crate::handlers::ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "partition_unavailable",
            format!("This belongs to partition {}, and no hex of it is reachable now. Nothing was written; try again shortly.", partition),
        )
        .with_retry_after(2)
        .into_response();
    }
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
                    if let Some(next) = overseer_for(&engine, until, elsewhere).await {
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
    if status != StatusCode::MISDIRECTED_REQUEST || epoch_refusals >= 3 || !headers.contains_key(PARTITION_EPOCH_HEADER) {
        break (status, out_headers, answer);
    }
    // Only a refusal signed for this very request proves nothing ran; without that
    // proof, only a read or a keyed write (the Overseer replays it) is sent again.
    let (body, verified) = match answer {
        Answer::Checked(body) => (body, overseer.signs_forwarded),
        Answer::Unchecked(response) => (response.bytes().await.unwrap_or_default(), false),
    };
    if !is_epoch_mismatch(status, &body) || !(verified || read || resendable) {
        break (status, out_headers, Answer::Checked(body));
    }
    // Refused untouched for a stale partition map: reload it and send again, a few times at most.
    epoch_refusals += 1;
    debug!("The Overseer '{}' routes by another partition map epoch; reloading this hex's and sending {} {} again.", overseer.name, parts.method, path);
    engine.refresh_partition_map().await;
    signature = sign_forward(&engine, parts.method.as_str(), &path_and_query, &client, &mut headers).await;
    tokio::time::sleep(Duration::from_millis(100 * epoch_refusals)).await;
    };
    respond(status, out_headers, answer, &overseer)
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

// ---------------------------------------------------------------------------
// Reads across partitions (partitioning stage 2, a preview)
// ---------------------------------------------------------------------------

/// Marks a read one partition answers alone: a part of a fan-out (or a client asking
/// for this hex's partition only).
pub const PARTITION_LOCAL_HEADER: &str = "x-hexdb-partition-local";

/// A read a partitioned lattice answers from every partition.
enum FanOut {
    /// `GET /{tess}` or `POST /{tess}/_query`, as one query: sort keys, page, fields, filter.
    Query { tess: String, filter: serde_json::Value, sort: Vec<hexdb_core::SortKey>, limit: usize, offset: usize, fields: Option<Vec<String>>, total: bool },
    /// `GET /{tess}/count`.
    Count { tess: String, query: String },
}

fn not_across_partitions(what: &str) -> Box<Response> {
    Box::new(crate::handlers::ApiError::new(
        StatusCode::CONFLICT,
        "cross_partition",
        format!("{} isn't answered across partitions yet (partitioning is a preview); send it with the {} header to read this hex's partition only.", what, PARTITION_LOCAL_HEADER),
    )
    .into_response())
}

fn page_bounds(limit: Option<usize>, offset: Option<usize>) -> Result<(usize, usize), Box<Response>> {
    let (limit, offset) = (limit.unwrap_or(crate::handlers::DEFAULT_PAGE_SIZE), offset.unwrap_or(0));
    if limit + offset > crate::handlers::MAX_PAGE_SIZE {
        return Err(Box::new(crate::handlers::ApiError::invalid(format!("Across partitions, offset + limit can't pass {} yet (partitioning is a preview).", crate::handlers::MAX_PAGE_SIZE)).into_response()));
    }
    Ok((limit, offset))
}

fn no_cursor() -> Box<Response> {
    Box::new(crate::handlers::ApiError::invalid("Across partitions, page with offset: after isn't available yet (partitioning is a preview).").into_response())
}

/// What a read needs from every partition, or None for a read one hex answers (or that
/// isn't partitioned at all). Reads that can't be merged yet are refused rather than
/// answered from one partition: never a partial answer passed off as whole.
fn classify_read(engine: &HexDBEngine, method: &Method, path: &str, query: Option<&str>, body: &[u8]) -> Result<Option<FanOut>, Box<Response>> {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let first = segments.first().copied().unwrap_or_default();
    if (path == "/sql" || path == "/graphql") && *method == Method::POST {
        return Err(not_across_partitions(if path == "/sql" { "SQL" } else { "GraphQL" }));
    }
    if first.is_empty() || hexdb_core::catalog::is_route_name(first) || first.starts_with('_') || engine.is_system_tessellation(first) {
        return Ok(None);
    }
    let tess = first.to_string();
    match (method.clone(), segments.len(), segments.get(1).copied()) {
        (Method::GET, 1, _) => {
            let uri: axum::http::Uri = format!("http://h{}?{}", path, query.unwrap_or_default()).parse().unwrap_or_default();
            let params = axum::extract::Query::<crate::handlers::ListParams>::try_from_uri(&uri).map_err(|e| Box::new(crate::handlers::ApiError::invalid(e.body_text()).into_response()))?.0;
            if params.after.is_some() {
                return Err(no_cursor());
            }
            let (limit, offset) = page_bounds(params.limit, params.offset)?;
            Ok(Some(FanOut::Query {
                tess,
                filter: crate::handlers::parse_filter_param(params.filter.as_deref()).map_err(|e| Box::new(e.into_response()))?,
                sort: crate::handlers::parse_sort_text(params.sort.as_deref().unwrap_or("")).map_err(|e| Box::new(e.into_response()))?,
                limit,
                offset,
                fields: params.fields.as_deref().map(crate::handlers::split_fields),
                total: params.total.unwrap_or(true),
            }))
        }
        (Method::POST, 2, Some("_query")) => {
            // A body the handler can't read: it explains what's wrong with it.
            let Ok(request) = serde_json::from_slice::<crate::handlers::QueryRequest>(body) else { return Ok(None) };
            if request.near.is_some() {
                return Err(not_across_partitions("A similarity search (near)"));
            }
            if request.after.is_some() {
                return Err(no_cursor());
            }
            let sort = match request.sort {
                Some(crate::handlers::SortSpec::Text(t)) => crate::handlers::parse_sort_text(&t).map_err(|e| Box::new(e.into_response()))?,
                Some(crate::handlers::SortSpec::Keys(k)) => k,
                None => Vec::new(),
            };
            let (limit, offset) = page_bounds(request.limit, request.offset)?;
            Ok(Some(FanOut::Query { tess, filter: request.filter, sort, limit, offset, fields: request.fields.map(crate::handlers::FieldList::paths), total: request.total.unwrap_or(true) }))
        }
        (Method::GET, 2, Some("count")) => Ok(Some(FanOut::Count { tess, query: query.unwrap_or_default().to_string() })),
        (Method::POST, 2, Some("_aggregate")) => Err(not_across_partitions("An aggregation")),
        _ => Ok(None),
    }
}

/// On a partitioned lattice, answers a read from every partition and merges the answers.
/// Gives the request back when it isn't such a read.
async fn fan_out_read(engine: &Arc<HexDBEngine>, request: Request, next: &Next) -> Result<Response, Box<Request>> {
    let map = engine.partition_map();
    if map.is_single() || request.headers().contains_key(PARTITION_LOCAL_HEADER) || !matches!(*request.method(), Method::GET | Method::POST) {
        return Err(Box::new(request));
    }
    let path = request.uri().path().to_string();
    if *request.method() == Method::GET && path == "/changes" {
        let (parts, _) = request.into_parts();
        return Ok(merged_changes(engine, parts, next).await);
    }
    if *request.method() == Method::GET && (path == "/changes/stream" || path == "/changes/ws") {
        return Ok(*not_across_partitions("A live change stream (Server-Sent Events or WebSocket)"));
    }
    // Only these reads are partitioned; anything else goes on untouched (with its body).
    let get_list = *request.method() == Method::GET && path.trim_start_matches('/').split('/').count() == 1;
    if !(path == "/sql" || path == "/graphql" || path.ends_with("/_query") || path.ends_with("/_aggregate") || path.ends_with("/count") || get_list) {
        return Err(Box::new(request));
    }
    let (parts, body) = request.into_parts();
    let bytes: Bytes = match axum::body::to_bytes(body, crate::routes::bulk_body_limit(engine)).await {
        Ok(bytes) => bytes,
        Err(_) => return Ok(crate::handlers::ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "The request body is too large.").into_response()),
    };
    let plan = match classify_read(engine, &parts.method, &path, parts.uri.query(), &bytes) {
        Ok(Some(plan)) => plan,
        Ok(None) => return Err(Box::new(Request::from_parts(parts, Body::from(bytes)))),
        Err(refused) => return Ok(*refused),
    };
    // Each partition answers the same narrow request: a query's first offset + limit
    // matches in the query's order (whole documents, projected after the merge), or a count.
    let (sub_path, sub_method, sub_body) = match &plan {
        FanOut::Query { tess, filter, sort, limit, offset, total, .. } => (
            format!("/{}/_query", tess),
            Method::POST,
            Bytes::from(serde_json::to_vec(&serde_json::json!({ "filter": filter, "sort": sort, "limit": limit + offset, "offset": 0, "total": total })).unwrap_or_default()),
        ),
        FanOut::Count { tess, query } => (if query.is_empty() { format!("/{}/count", tess) } else { format!("/{}/count?{}", tess, query) }, Method::GET, Bytes::new()),
    };
    let mine = engine.my_partition();
    let mut answers: Vec<serde_json::Value> = Vec::new();
    for partition in map.partitions.iter().map(|p| p.id) {
        let (status, body) = if Some(partition) == mine {
            let mut local = parts.clone();
            local.method = sub_method.clone();
            local.uri = sub_path.parse().unwrap_or_default();
            local.headers.insert(HeaderName::from_static(PARTITION_LOCAL_HEADER), HeaderValue::from_static("1"));
            local.headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            local.headers.remove(header::CONTENT_LENGTH);
            let response = next.clone().run(Request::from_parts(local, Body::from(sub_body.clone()))).await;
            let status = response.status();
            (status, axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap_or_default())
        } else {
            match ask_partition(engine, partition, &parts, &sub_method, &sub_path, &sub_body).await {
                Ok(answer) => answer,
                Err(response) => return Ok(*response),
            }
        };
        if !status.is_success() {
            // A partition's own refusal (permissions, a bad filter): the client sees it as is.
            return Ok((status, [(header::CONTENT_TYPE, "application/json")], body).into_response());
        }
        answers.push(serde_json::from_slice(&body).unwrap_or_default());
    }
    Ok(merge(plan, answers, map.partitions.len()))
}

/// Sends one partition its part of a fan-out, through that partition's Overseer, signed as
/// a forwarded request with the client's credentials.
async fn ask_partition(engine: &HexDBEngine, partition: u32, parts: &axum::http::request::Parts, method: &Method, path: &str, body: &Bytes) -> Result<(StatusCode, Bytes), Box<Response>> {
    let unavailable = || {
        Box::new(crate::handlers::ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "partition_unavailable", format!("Partition {} isn't reachable now, so the lattice can't answer this whole; try again shortly.", partition))
            .with_retry_after(2)
            .into_response())
    };
    let Some(overseer) = overseer_for(engine, Instant::now() + Duration::from_secs(5), Some(partition)).await else { return Err(unavailable()) };
    let Some(http) = forward_client(engine) else { return Err(unavailable()) };
    let client = crate::auth::client_address(&parts.extensions);
    let mut headers = HeaderMap::new();
    copy_headers(&parts.headers, &mut headers);
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(HeaderName::from_static(PARTITION_LOCAL_HEADER), HeaderValue::from_static("1"));
    let signature = sign_forward(engine, method.as_str(), path, &client, &mut headers).await;
    let url = format!("{}{}", hexdb_core::replication::peer_base_url(&overseer), path);
    let method = reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let timeout = Duration::from_secs(engine.config.limits.request_timeout_seconds.max(1));
    let response = match http.request(method, &url).timeout(timeout).headers(headers).body(body.clone()).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("⚠️ Asking partition {} ('{}') failed: {}", partition, overseer.name, e);
            return Err(unavailable());
        }
    };
    let (status, _, answer) = checked_answer(engine, &overseer, &signature, response, None).await.map_err(|e| Box::new(e.into_response()))?;
    let body = match answer {
        Answer::Checked(body) => body,
        Answer::Unchecked(response) => response.bytes().await.unwrap_or_default(),
    };
    Ok((status, body))
}

/// Merges the partitions' answers into one, as a one-partition lattice would answer.
fn merge(plan: FanOut, answers: Vec<serde_json::Value>, partitions: usize) -> Response {
    match plan {
        FanOut::Count { .. } => {
            let count: u64 = answers.iter().map(|a| a["count"].as_u64().unwrap_or(0)).sum();
            axum::Json(serde_json::json!({ "count": count, "partitions": partitions })).into_response()
        }
        FanOut::Query { sort, limit, offset, fields, total, .. } => {
            let mut documents: Vec<serde_json::Value> = answers.iter().flat_map(|a| a["documents"].as_array().cloned().unwrap_or_default()).collect();
            documents.sort_by(|a, b| hexdb_core::filter::compare_api_documents(a, b, &sort));
            let page: Vec<serde_json::Value> = documents
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|d| match &fields {
                    Some(f) if !f.is_empty() => hexdb_core::project(&d, f),
                    _ => d,
                })
                .collect();
            let total = if total { answers.iter().map(|a| a["total"].as_u64()).sum::<Option<u64>>() } else { None };
            axum::Json(serde_json::json!({ "documents": page, "total": total, "next": null, "partitions": partitions })).into_response()
        }
    }
}

/// A merged change feed's cursor: each partition's last sequence number, as
/// `"0:12,1:40"`. A partition it doesn't name starts at its current position.
fn parse_change_cursor(text: &str) -> Result<std::collections::BTreeMap<u32, u64>, Box<Response>> {
    let bad = || Box::new(crate::handlers::ApiError::invalid("after: on a partitioned lattice, pass back the cursor a merged feed answered (\"0:12,1:40\").").into_response());
    let mut cursor = std::collections::BTreeMap::new();
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (partition, seq) = part.split_once(':').ok_or_else(bad)?;
        cursor.insert(partition.parse::<u32>().map_err(|_| bad())?, seq.parse::<u64>().map_err(|_| bad())?);
    }
    Ok(cursor)
}

fn format_change_cursor(cursor: &std::collections::BTreeMap<u32, u64>) -> String {
    cursor.iter().map(|(p, s)| format!("{}:{}", p, s)).collect::<Vec<_>>().join(",")
}

/// `GET /changes` on a partitioned lattice: every partition's feed, read from that
/// partition's Overseer (whose sequence numbers are the partition's history), merged.
/// Order holds within a partition; each change says which partition it's from.
async fn merged_changes(engine: &Arc<HexDBEngine>, parts: axum::http::request::Parts, next: &Next) -> Response {
    #[derive(serde::Deserialize, Default)]
    #[serde(deny_unknown_fields)]
    struct Params {
        after: Option<String>,
        tessellation: Option<String>,
        limit: Option<usize>,
        wait: Option<u64>,
    }
    let params = match axum::extract::Query::<Params>::try_from_uri(&parts.uri) {
        Ok(q) => q.0,
        Err(e) => return crate::handlers::ApiError::invalid(e.body_text()).into_response(),
    };
    let mut cursor = match params.after.as_deref() {
        Some(text) => match parse_change_cursor(text) {
            Ok(c) => c,
            Err(refused) => return *refused,
        },
        None => Default::default(),
    };
    let map = engine.partition_map();
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);
    let deadline = Instant::now() + Duration::from_secs(params.wait.unwrap_or(0).min(60));
    loop {
        let mut changes: Vec<serde_json::Value> = Vec::new();
        for partition in map.partitions.iter().map(|p| p.id) {
            let mut path = format!("/changes?limit={}", limit);
            if let Some(seq) = cursor.get(&partition) {
                path.push_str(&format!("&after={}", seq));
            }
            if let Some(t) = params.tessellation.as_deref().filter(|t| !t.is_empty()) {
                path.push_str(&format!("&tessellation={}", hexdb_core::sigv4::uri_encode(t)));
            }
            // This hex's own partition answers here only if it's that partition's Overseer:
            // a replica's sequence numbers are its own, not the partition's.
            let (status, body) = if Some(partition) == engine.my_partition() && engine.is_writable() {
                let mut local = parts.clone();
                local.uri = path.parse().unwrap_or_default();
                local.headers.insert(HeaderName::from_static(PARTITION_LOCAL_HEADER), HeaderValue::from_static("1"));
                let response = next.clone().run(Request::from_parts(local, Body::empty())).await;
                let status = response.status();
                (status, axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024).await.unwrap_or_default())
            } else {
                match ask_partition(engine, partition, &parts, &Method::GET, &path, &Bytes::new()).await {
                    Ok(answer) => answer,
                    Err(response) => return *response,
                }
            };
            if !status.is_success() {
                return (status, [(header::CONTENT_TYPE, "application/json")], body).into_response();
            }
            let answer: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            for mut change in answer["changes"].as_array().cloned().unwrap_or_default() {
                change["partition"] = serde_json::json!(partition);
                changes.push(change);
            }
            if let Some(seq) = answer["last_seq"].as_u64() {
                cursor.insert(partition, seq);
            }
        }
        if !changes.is_empty() || Instant::now() >= deadline {
            let token = format_change_cursor(&cursor);
            return axum::Json(serde_json::json!({ "changes": changes, "last_seq": token, "cursor": token, "partitions": map.partitions.len() })).into_response();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
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
