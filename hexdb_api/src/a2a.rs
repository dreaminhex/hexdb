// HexDB API: A2A (Agent2Agent)
//
// Other agents can delegate questions about the lattice's data to it over the
// A2A protocol (version 1.0, JSON-RPC binding; the 0.3 method names are
// accepted too). `GET /.well-known/agent-card.json` describes the agent
// (public, as discovery needs; only when `a2a.enabled`), and `POST /a2a`
// takes JSON-RPC requests:
//
//   SendMessage    ask a question; answered at once as a completed task
//   GetTask        a task this caller started (kept for an hour)
//   ListTasks      this caller's tasks, newest first
//   CancelTask     tasks finish before the answer, so: not cancelable
//
// Streaming and push notifications aren't offered. Every message runs the
// chat (`crate::agent::answer`) as the caller, authenticated by an API key
// or session token: the `chat` permission, read permissions, row filters and
// field masks, the `ai.chat.*` limits and the monthly budget all apply, and
// each answer is audited as `a2a.message`. A context (`contextId`) carries
// the conversation, so follow-up questions work; contexts and tasks belong
// to the user who made them and live in memory on the Overseer (A2A
// requests are forwarded there, like the chat).

use crate::agent::{self, ChatHistoryItem};
use crate::auth::Auth;
use crate::handlers::{ApiError, ApiResult, Engine};
use axum::{
    extract::rejection::JsonRejection,
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use chrono::{SecondsFormat, Utc};
use hexdb_core::{Action, Principal};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const PROTOCOL_VERSION: &str = "1.0";
/// How long tasks and contexts are kept after their last use.
const KEEP: Duration = Duration::from_secs(3600);
/// Most tasks (and contexts) kept in all.
const MAX_KEPT: usize = 2000;
/// Longest context or task ID a client may name.
const MAX_ID: usize = 128;

// JSON-RPC error codes (A2A section 5.4, and JSON-RPC's own).
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const TASK_NOT_FOUND: i64 = -32001;
const TASK_NOT_CANCELABLE: i64 = -32002;
const PUSH_NOT_SUPPORTED: i64 = -32003;
const UNSUPPORTED: i64 = -32004;
const CONTENT_TYPE_NOT_SUPPORTED: i64 = -32005;
const VERSION_NOT_SUPPORTED: i64 = -32009;

fn enabled(engine: &hexdb_core::engine::HexDBEngine) -> bool {
    engine.config_value("a2a.enabled") == json!(true)
}

/// `GET /.well-known/agent-card.json`: the agent card (404 while A2A is off).
pub async fn agent_card(axum::extract::State(engine): Engine, Extension(https): Extension<crate::client::ViaHttps>, headers: HeaderMap) -> ApiResult {
    if !enabled(&engine) {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found", "A2A is off on this lattice."));
    }
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or(&engine.config.network.api_endpoint);
    let url = format!("{}://{}/a2a", if https.0 { "https" } else { "http" }, host);
    let card = json!({
        "name": "HexDB",
        "description": "Answers questions about the data stored in this HexDB lattice, looking it up as the calling user: their read permissions, row filters and field masks apply. Read-only.",
        "supportedInterfaces": [{ "url": url, "protocolBinding": "JSONRPC", "protocolVersion": PROTOCOL_VERSION }],
        "version": env!("CARGO_PKG_VERSION"),
        "documentationUrl": "https://github.com/dreaminhex/hexdb/blob/main/MANUAL.md",
        "capabilities": { "streaming": false, "pushNotifications": false, "extendedAgentCard": false },
        "securitySchemes": {
            "bearer": { "httpAuthSecurityScheme": { "scheme": "Bearer", "description": "A HexDB API key (hxk_...) or session token, sent as Authorization: Bearer." } }
        },
        "securityRequirements": [{ "schemes": { "bearer": { "list": [] } } }],
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": [{
            "id": "data-questions",
            "name": "Questions about the data",
            "description": "Answers a question about the documents and files in this lattice by querying, searching, counting and aggregating them. Follow-up questions in the same context see the earlier ones.",
            "tags": ["database", "data", "query", "search"],
            "examples": ["Which tessellations are there, and how many documents does each have?", "How many orders over $100 shipped last month?"]
        }]
    });
    Ok(([(header::CACHE_CONTROL, "public, max-age=300")], Json(card)).into_response())
}

/// One task: its owner and its A2A (1.0) form.
struct StoredTask {
    owner: String,
    task: Value,
    at: Instant,
}

/// A conversation: its owner, the questions and answers so far.
struct Context {
    owner: String,
    history: Vec<ChatHistoryItem>,
    at: Instant,
}

#[derive(Default)]
struct Store {
    tasks: HashMap<String, StoredTask>,
    contexts: HashMap<String, Context>,
}

impl Store {
    /// Forget what's older than an hour, and the oldest beyond `MAX_KEPT`.
    fn prune(&mut self) {
        fn trim<T>(map: &mut HashMap<String, T>, at: impl Fn(&T) -> Instant) {
            let now = Instant::now();
            map.retain(|_, v| now.duration_since(at(v)) < KEEP);
            if map.len() > MAX_KEPT {
                let mut ages: Vec<(Instant, String)> = map.iter().map(|(k, v)| (at(v), k.clone())).collect();
                ages.sort();
                for (_, key) in ages.into_iter().take(map.len() - MAX_KEPT) {
                    map.remove(&key);
                }
            }
        }
        trim(&mut self.tasks, |t| t.at);
        trim(&mut self.contexts, |c| c.at);
    }
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(Default::default)
}

/// The protocol a request speaks: 1.0, or 0.3 (its method names and shapes).
#[derive(Clone, Copy, PartialEq)]
enum Dialect {
    V1,
    V03,
}

fn reply(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `POST /a2a`: one JSON-RPC request.
pub async fn post(
    axum::extract::State(engine): Engine,
    Auth(principal): Auth,
    crate::client::ClientIp(client): crate::client::ClientIp,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult {
    if !enabled(&engine) {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found", "A2A is off on this lattice. An administrator can turn it on (a2a.enabled)."));
    }
    let Json(request) = body?;
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return Ok(Json(error(&id, INVALID_REQUEST, "Send one JSON-RPC 2.0 request with a method.")).into_response());
    };
    let version = headers.get("a2a-version").and_then(|v| v.to_str().ok()).map(str::trim).unwrap_or_default();
    if !matches!(version, "" | "0.3" | "1.0") && !version.starts_with("1.") && !version.starts_with("0.3") {
        return Ok(Json(error(&id, VERSION_NOT_SUPPORTED, "This agent speaks A2A 1.0 (and 0.3's method names).")).into_response());
    }
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let (dialect, method) = match method {
        "message/send" => (Dialect::V03, "SendMessage"),
        "tasks/get" => (Dialect::V03, "GetTask"),
        "tasks/cancel" => (Dialect::V03, "CancelTask"),
        "message/stream" | "tasks/resubscribe" => (Dialect::V03, "SendStreamingMessage"),
        m if m.starts_with("tasks/pushNotificationConfig/") => (Dialect::V03, "SetTaskPushNotificationConfig"),
        m => (Dialect::V1, m),
    };
    let answer = match method {
        "SendMessage" => send_message(&engine, &principal, &client, &id, &params, dialect).await?,
        "GetTask" => get_task(&principal, &id, &params, dialect),
        "ListTasks" => list_tasks(&principal, &id, &params),
        "CancelTask" => {
            let task_id = params["id"].as_str().unwrap_or_default();
            if owned_task(&principal, task_id).is_some() {
                error(&id, TASK_NOT_CANCELABLE, "The task has already completed.")
            } else {
                error(&id, TASK_NOT_FOUND, "No such task.")
            }
        }
        "SendStreamingMessage" | "SubscribeToTask" => error(&id, UNSUPPORTED, "Streaming isn't offered; use SendMessage."),
        m if m.contains("PushNotificationConfig") => error(&id, PUSH_NOT_SUPPORTED, "Push notifications aren't offered."),
        "GetExtendedAgentCard" => error(&id, UNSUPPORTED, "There is no extended agent card."),
        other => error(&id, METHOD_NOT_FOUND, &format!("Method '{}' isn't supported.", other)),
    };
    Ok(Json(answer).into_response())
}

/// The text of a message's parts (1.0 `{"text"}`, or 0.3 `{"kind": "text", "text"}`).
fn message_text(message: &Value) -> Option<String> {
    let parts = message.get("parts")?.as_array()?;
    let texts: Vec<&str> = parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

fn client_id(value: &Value) -> Result<Option<String>, &'static str> {
    match value.as_str() {
        None => Ok(None),
        Some(s) if !s.is_empty() && s.len() <= MAX_ID && s.chars().all(|c| c.is_ascii_graphic()) => Ok(Some(s.to_string())),
        Some(_) => Err("IDs are 1-128 printable characters."),
    }
}

async fn send_message(engine: &std::sync::Arc<hexdb_core::engine::HexDBEngine>, principal: &Principal, client: &str, id: &Value, params: &Value, dialect: Dialect) -> Result<Value, ApiError> {
    principal.require_action(Action::Chat)?;
    if engine.config_value("ai.chat.enabled") != json!(true) {
        return Err(ApiError::forbidden("A2A answers with the AI chat, which is off. An administrator can turn it on (ai.chat.enabled)."));
    }
    let message = &params["message"];
    if !message.is_object() {
        return Ok(error(id, INVALID_PARAMS, "params.message is required."));
    }
    if !matches!(message["role"].as_str(), Some("ROLE_USER") | Some("user")) {
        return Ok(error(id, INVALID_PARAMS, "The message's role must be ROLE_USER."));
    }
    let Some(question) = message_text(message) else {
        return Ok(error(id, CONTENT_TYPE_NOT_SUPPORTED, "Only text parts are understood."));
    };
    if !message["taskId"].is_null() {
        return Ok(error(id, UNSUPPORTED, "Tasks here complete with their answer and take no more messages. Send a new message with the same contextId to follow up."));
    }
    let context_id = match client_id(&message["contextId"]) {
        Ok(found) => found.unwrap_or_else(|| ulid::Ulid::generate().to_string()),
        Err(e) => return Ok(error(id, INVALID_PARAMS, e)),
    };
    let message_id = message["messageId"].as_str().map(String::from).unwrap_or_else(|| ulid::Ulid::generate().to_string());
    let history_length = params["configuration"]["historyLength"].as_u64();

    // The conversation so far, if this caller started it.
    let history: Vec<ChatHistoryItem> = {
        let mut store = store().lock().unwrap();
        store.prune();
        match store.contexts.get(&context_id) {
            Some(c) if c.owner == principal.user_id => c.history.iter().map(|h| ChatHistoryItem { role: h.role.clone(), content: h.content.clone() }).collect(),
            Some(_) => return Ok(error(id, INVALID_PARAMS, "That contextId belongs to someone else; start a new context.")),
            None => Vec::new(),
        }
    };

    let task_id = ulid::Ulid::generate().to_string();
    let user_message = json!({ "messageId": message_id, "role": "ROLE_USER", "parts": [{ "text": question }], "contextId": context_id, "taskId": task_id });
    let (state, text, metadata) = match agent::answer(engine, principal, client, &question, &history, "a2a.message").await {
        Ok(answer) => {
            let mut store = store().lock().unwrap();
            let context = store.contexts.entry(context_id.clone()).or_insert_with(|| Context { owner: principal.user_id.clone(), history: Vec::new(), at: Instant::now() });
            context.history.push(ChatHistoryItem { role: "user".into(), content: question.clone() });
            context.history.push(ChatHistoryItem { role: "assistant".into(), content: answer.answer.clone() });
            context.at = Instant::now();
            let metadata = json!({ "hexdb": { "model": answer.model, "tokens": { "input": answer.input_tokens, "output": answer.output_tokens }, "lookups": answer.steps.len() } });
            ("TASK_STATE_COMPLETED", answer.answer, metadata)
        }
        // Permission and input problems are the request's fault; the rest fail the task.
        Err(e) if matches!(e.status(), StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED) => return Err(e),
        Err(e) if e.status() == StatusCode::BAD_REQUEST => return Ok(error(id, INVALID_PARAMS, e.message())),
        Err(e) => ("TASK_STATE_FAILED", e.message().to_string(), json!({ "hexdb": { "error": e.code() } })),
    };
    let agent_message = json!({ "messageId": ulid::Ulid::generate().to_string(), "role": "ROLE_AGENT", "parts": [{ "text": text }], "contextId": context_id, "taskId": task_id });
    let mut task = json!({
        "id": task_id,
        "contextId": context_id,
        "status": { "state": state, "message": agent_message, "timestamp": now() },
        "history": [user_message, agent_message],
        "metadata": metadata,
    });
    if state == "TASK_STATE_COMPLETED" {
        task["artifacts"] = json!([{ "artifactId": ulid::Ulid::generate().to_string(), "name": "answer", "parts": [{ "text": text }] }]);
    }
    {
        let mut store = store().lock().unwrap();
        store.tasks.insert(task_id, StoredTask { owner: principal.user_id.clone(), task: task.clone(), at: Instant::now() });
    }
    Ok(reply(id, shaped(&task, history_length, dialect)))
}

fn owned_task(principal: &Principal, task_id: &str) -> Option<Value> {
    let mut store = store().lock().unwrap();
    store.prune();
    store.tasks.get(task_id).filter(|t| t.owner == principal.user_id).map(|t| t.task.clone())
}

fn get_task(principal: &Principal, id: &Value, params: &Value, dialect: Dialect) -> Value {
    match owned_task(principal, params["id"].as_str().unwrap_or_default()) {
        // GetTask answers with the task itself, in either version.
        Some(task) => {
            let task = trimmed(task, params["historyLength"].as_u64());
            reply(id, if dialect == Dialect::V03 { to_v03(task) } else { task })
        }
        None => error(id, TASK_NOT_FOUND, "No such task (tasks are kept for an hour)."),
    }
}

fn list_tasks(principal: &Principal, id: &Value, params: &Value) -> Value {
    let context = params["contextId"].as_str();
    let page_size = params["pageSize"].as_u64().unwrap_or(50).clamp(1, 100) as usize;
    let mut store = store().lock().unwrap();
    store.prune();
    let mut tasks: Vec<(Instant, Value)> = store
        .tasks
        .values()
        .filter(|t| t.owner == principal.user_id && context.is_none_or(|c| t.task["contextId"] == c))
        .map(|t| (t.at, t.task.clone()))
        .collect();
    tasks.sort_by_key(|t| std::cmp::Reverse(t.0));
    let total = tasks.len();
    let tasks: Vec<Value> = tasks.into_iter().take(page_size).map(|(_, t)| t).collect();
    reply(id, json!({ "tasks": tasks, "nextPageToken": "", "pageSize": page_size, "totalSize": total }))
}

/// A task as a SendMessage result: 1.0 wraps it (`{"task"}`); 0.3 returns it
/// bare, with `kind` discriminators and lowercase states and roles.
fn shaped(task: &Value, history_length: Option<u64>, dialect: Dialect) -> Value {
    let task = trimmed(task.clone(), history_length);
    match dialect {
        Dialect::V1 => json!({ "task": task }),
        Dialect::V03 => to_v03(task),
    }
}

/// A task with at most `history_length` of its latest messages.
fn trimmed(mut task: Value, history_length: Option<u64>) -> Value {
    if let (Some(n), Some(history)) = (history_length, task["history"].as_array_mut()) {
        let skip = history.len().saturating_sub(n as usize);
        history.drain(..skip);
    }
    task
}

fn to_v03(mut task: Value) -> Value {
    fn message(m: &mut Value) {
        m["kind"] = json!("message");
        if let Some(role) = m["role"].as_str() {
            m["role"] = json!(if role == "ROLE_AGENT" { "agent" } else { "user" });
        }
        parts(&mut m["parts"]);
    }
    fn parts(p: &mut Value) {
        for part in p.as_array_mut().into_iter().flatten() {
            part["kind"] = json!("text");
        }
    }
    task["kind"] = json!("task");
    if let Some(state) = task["status"]["state"].as_str() {
        task["status"]["state"] = json!(state.trim_start_matches("TASK_STATE_").to_ascii_lowercase());
    }
    message(&mut task["status"]["message"]);
    for m in task["history"].as_array_mut().into_iter().flatten() {
        message(m);
    }
    for a in task["artifacts"].as_array_mut().into_iter().flatten() {
        parts(&mut a["parts"]);
    }
    task
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_parts_in_both_dialects() {
        assert_eq!(message_text(&json!({ "parts": [{ "text": "a" }, { "url": "x" }, { "kind": "text", "text": "b" }] })), Some("a\nb".into()));
        assert_eq!(message_text(&json!({ "parts": [{ "url": "x" }] })), None);
    }

    #[test]
    fn tasks_take_the_0_3_shape_for_0_3_methods() {
        let task = json!({
            "id": "t", "contextId": "c",
            "status": { "state": "TASK_STATE_COMPLETED", "message": { "role": "ROLE_AGENT", "parts": [{ "text": "hi" }] } },
            "history": [{ "role": "ROLE_USER", "parts": [{ "text": "q" }] }, { "role": "ROLE_AGENT", "parts": [{ "text": "hi" }] }],
            "artifacts": [{ "artifactId": "a", "parts": [{ "text": "hi" }] }]
        });
        let v1 = shaped(&task, Some(1), Dialect::V1);
        assert_eq!(v1["task"]["history"].as_array().unwrap().len(), 1);
        let old = shaped(&task, None, Dialect::V03);
        assert_eq!(old["kind"], "task");
        assert_eq!(old["status"]["state"], "completed");
        assert_eq!(old["history"][0]["role"], "user");
        assert_eq!(old["history"][1]["kind"], "message");
        assert_eq!(old["artifacts"][0]["parts"][0]["kind"], "text");
    }
}
