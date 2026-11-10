// HexDB API: the Oracle, the lattice's AI hex
//
// A hex with `identity.role = "oracle"` never leads and keeps no user data
// (only the system tessellations: users, roles, configuration). While one is
// live, the other hexes pass it every AI question: MCP (`/mcp`), the chat
// (`/ai/chat`) and A2A (`/a2a`, and its tasks). It runs the model and the
// tool-use loop there, so the waiting on the AI provider and the chat's
// per-user and per-address limits stay off the hexes that hold the data.
//
// Each lookup the model asks for (`crate::agent`'s tools) goes to a hex that
// holds every slice, preferring a replica to the Overseer: a signed
// `POST /lattice/ai-tool` naming the user, which that hex runs as the user
// (their permissions, row filters and field masks, as they stand there), and
// answers with the result. The Oracle cuts results to size as before.
//
// Without a live Oracle every hex answers AI questions itself, as before: an
// Oracle takes load off the lattice, and AI doesn't stop when it does.

use crate::handlers::{ApiError, ApiResult, Engine};
use axum::{
    body::Bytes,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::IntoResponse,
    Json,
};
use hexdb_core::{
    engine::HexDBEngine,
    network::discovery::{PeerHex, ROLE_ORACLE, ROLE_OVERSEER},
    Principal,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tracing::warn;

/// The lattice route the Oracle runs tools through.
pub const TOOL_PATH: &str = "/lattice/ai-tool";
/// Longest a tool may take on the hex that runs it.
const TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// True for the requests an Oracle answers: AI questions.
pub fn is_ai_question(method: &Method, path: &str) -> bool {
    (path == "/mcp" && method == Method::POST) || path == "/ai/chat" || path == "/a2a" || path.starts_with("/a2a/")
}

/// A live Oracle, if the lattice has one (the same one from every hex while the view agrees).
pub async fn live_oracle(engine: &HexDBEngine) -> Option<PeerHex> {
    let me = engine.id.to_string();
    let peers = engine.peers.lock().await;
    peers.iter().filter(|p| p.status == "active" && p.hex.role == ROLE_ORACLE && p.hex.id != me).map(|p| p.hex.clone()).min_by(|a, b| a.id.cmp(&b.id))
}

/// Hexes that can run a tool (they hold every slice), replicas first so the
/// Overseer takes the fewest; spread across the replicas in turn.
async fn data_hexes(engine: &HexDBEngine) -> Vec<PeerHex> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let peers = engine.peers.lock().await;
    let mut replicas: Vec<PeerHex> = peers
        .iter()
        .filter(|p| p.status == "active" && p.hex.role != ROLE_ORACLE && p.hex.role != ROLE_OVERSEER && p.hex.slices_held == hexdb_core::slicing::ALL_SLICES)
        .map(|p| p.hex.clone())
        .collect();
    replicas.sort_by(|a, b| a.id.cmp(&b.id));
    if !replicas.is_empty() {
        let turn = NEXT.fetch_add(1, Ordering::Relaxed) % replicas.len();
        replicas.rotate_left(turn);
    }
    replicas.extend(peers.iter().filter(|p| p.status == "active" && p.hex.role == ROLE_OVERSEER).map(|p| p.hex.clone()));
    replicas
}

/// Run a tool for `principal` on a hex that holds the data (the Oracle has none).
pub async fn remote_tool(engine: &HexDBEngine, principal: &Principal, name: &str, input: &Value) -> Result<Value, String> {
    let hexes = data_hexes(engine).await;
    if hexes.is_empty() {
        return Err("No hex holding the lattice's data is reachable right now; try again shortly.".into());
    }
    let body = json!({ "user_id": principal.user_id, "tool": name, "input": input });
    let mut last = String::new();
    // A second hex only when the first couldn't be reached or answer.
    for hex in hexes.iter().take(2) {
        match hexdb_core::replication::post_to_peer_for(engine, hex, TOOL_PATH, &body, TOOL_TIMEOUT).await {
            Ok(bytes) => {
                let answer: Value = serde_json::from_slice(&bytes).map_err(|e| format!("'{}' answered unreadably: {}", hex.name, e))?;
                return match answer.get("error").and_then(Value::as_str) {
                    Some(error) => Err(error.to_string()),
                    None => Ok(answer.get("result").cloned().unwrap_or(Value::Null)),
                };
            }
            Err(e) => {
                warn!("⚠️ The Oracle couldn't run '{}' on '{}': {:#}", name, hex.name, e);
                last = format!("{:#}", e);
            }
        }
    }
    Err(format!("The hexes holding the data couldn't answer ({}); try again shortly.", last))
}

/// `POST /lattice/ai-tool`: run a tool for the Oracle, as the user it names.
pub async fn ai_tool(method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine, body: Bytes) -> ApiResult {
    crate::lattice::authorize(&engine, &headers, &method, &uri, &body)?;
    if engine.is_oracle() || engine.is_partial_replica() {
        return Err(ApiError::new(StatusCode::MISDIRECTED_REQUEST, "no_data", "This hex doesn't hold every slice, so it can't run tools for the Oracle."));
    }
    let request: Value = serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    let field = |k: &str| request[k].as_str().ok_or_else(|| ApiError::invalid(format!("{} is required.", k)));
    let (user_id, tool) = (field("user_id")?, field("tool")?);
    // The user as they stand on this hex: their current roles; refused if locked or gone.
    let principal = match hexdb_core::users::principal_for(&engine, user_id, "oracle").await {
        Ok(principal) => principal,
        Err(e) => return Ok(Json(json!({ "error": format!("{:#}", e) })).into_response()),
    };
    let input = request.get("input").cloned().unwrap_or(Value::Null);
    let answer = match crate::agent::run_tool_as(&engine, &principal, tool, &input).await {
        Ok(result) => json!({ "result": result }),
        Err(error) => json!({ "error": error }),
    };
    Ok(Json(answer).into_response())
}
