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
//   SendStreamingMessage   the same, as Server-Sent Events: the task, a
//                  status update per lookup, the answer, the final status
//   Create/Get/List/DeleteTaskPushNotificationConfig(s)   webhooks a task's
//                  result is POSTed to when it finishes (see `a2a_push`);
//                  SendMessage can set one, and with returnImmediately
//                  answers at once with the working task
//
// Every message runs the
// chat (`crate::agent::answer`) as the caller, authenticated by an API key
// or session token: the `chat` permission, read permissions, row filters and
// field masks, the `ai.chat.*` limits and the monthly budget all apply, and
// each answer is audited as `a2a.message`. A context (`contextId`) carries
// the conversation, so follow-up questions work; contexts and tasks belong
// to the user who made them and live in memory on the Overseer (A2A
// requests are forwarded there, like the chat).

use crate::agent::{self, ChatHistoryItem};
use crate::auth::Auth;
use crate::a2a_push::{self, PushConfig};
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
/// Most webhooks one task may have.
const MAX_PUSH_CONFIGS: usize = 10;
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
    // Either scheme will do: an API key or session token, or the OpenID Connect provider's access token.
    let mut security_schemes = json!({
        "bearer": { "httpAuthSecurityScheme": { "scheme": "Bearer", "description": "A HexDB API key (hxk_...) or session token, sent as Authorization: Bearer." } }
    });
    let mut requirements = vec![json!({ "schemes": { "bearer": { "list": [] } } })];
    if let Some(oauth) = hexdb_core::oauth::Settings::from_engine(&engine) {
        security_schemes["oidc"] = json!({ "openIdConnectSecurityScheme": {
            "openIdConnectUrl": oauth.discovery_url(),
            "description": format!("An access token from this provider for the audience {}, sent as Authorization: Bearer. It acts as the HexDB user its {} claim names.", oauth.audience, oauth.user_claim),
        } });
        requirements.push(json!({ "schemes": { "oidc": { "list": [] } } }));
    }
    let card = json!({
        "name": "HexDB",
        "description": "Answers questions about the data stored in this HexDB lattice, looking it up as the calling user: their read permissions, row filters and field masks apply. Read-only.",
        "supportedInterfaces": [{ "url": url, "protocolBinding": "JSONRPC", "protocolVersion": PROTOCOL_VERSION }],
        "version": env!("CARGO_PKG_VERSION"),
        "documentationUrl": "https://github.com/dreaminhex/hexdb/blob/main/MANUAL.md",
        "capabilities": { "streaming": true, "pushNotifications": true, "extendedAgentCard": false },
        "securitySchemes": security_schemes,
        "securityRequirements": requirements,
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
    /// Task ID -> its webhooks.
    push: HashMap<String, Vec<PushConfig>>,
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
        let tasks = &self.tasks;
        self.push.retain(|id, _| tasks.contains_key(id));
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
        "message/stream" => (Dialect::V03, "SendStreamingMessage"),
        "tasks/resubscribe" => (Dialect::V03, "SubscribeToTask"),
        "tasks/pushNotificationConfig/set" => (Dialect::V03, "CreateTaskPushNotificationConfig"),
        "tasks/pushNotificationConfig/get" => (Dialect::V03, "GetTaskPushNotificationConfig"),
        "tasks/pushNotificationConfig/list" => (Dialect::V03, "ListTaskPushNotificationConfigs"),
        "tasks/pushNotificationConfig/delete" => (Dialect::V03, "DeleteTaskPushNotificationConfig"),
        // An early 1.0 draft's name.
        "ListTaskPushNotificationConfig" => (Dialect::V1, "ListTaskPushNotificationConfigs"),
        m => (Dialect::V1, m),
    };
    if method == "SendStreamingMessage" {
        return send_streaming(engine, principal, client, id, params, dialect).await;
    }
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
        "SubscribeToTask" => error(&id, UNSUPPORTED, "Tasks here finish with their answer; read a task with GetTask."),
        "CreateTaskPushNotificationConfig" => create_push_config(&engine, &principal, &id, &params, dialect).await,
        "GetTaskPushNotificationConfig" => get_push_config(&principal, &id, &params, dialect),
        "ListTaskPushNotificationConfigs" => list_push_configs(&principal, &id, &params, dialect),
        "DeleteTaskPushNotificationConfig" => delete_push_config(&principal, &id, &params, dialect),
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

/// A SendMessage request, checked and ready to answer.
struct Prepared {
    question: String,
    history: Vec<ChatHistoryItem>,
    context_id: String,
    task_id: String,
    user_message: Value,
    history_length: Option<u64>,
}

/// Check a SendMessage request: Ok(Err(reply)) is a JSON-RPC error to send back.
fn prepare(engine: &hexdb_core::engine::HexDBEngine, principal: &Principal, id: &Value, params: &Value) -> Result<Result<Prepared, Value>, ApiError> {
    principal.require_action(Action::Chat)?;
    if engine.config_value("ai.chat.enabled") != json!(true) {
        return Err(ApiError::forbidden("A2A answers with the AI chat, which is off. An administrator can turn it on (ai.chat.enabled)."));
    }
    let message = &params["message"];
    if !message.is_object() {
        return Ok(Err(error(id, INVALID_PARAMS, "params.message is required.")));
    }
    if !matches!(message["role"].as_str(), Some("ROLE_USER") | Some("user")) {
        return Ok(Err(error(id, INVALID_PARAMS, "The message's role must be ROLE_USER.")));
    }
    let Some(question) = message_text(message) else {
        return Ok(Err(error(id, CONTENT_TYPE_NOT_SUPPORTED, "Only text parts are understood.")));
    };
    if !message["taskId"].is_null() {
        return Ok(Err(error(id, UNSUPPORTED, "Tasks here complete with their answer and take no more messages. Send a new message with the same contextId to follow up.")));
    }
    let context_id = match client_id(&message["contextId"]) {
        Ok(found) => found.unwrap_or_else(|| ulid::Ulid::generate().to_string()),
        Err(e) => return Ok(Err(error(id, INVALID_PARAMS, e))),
    };
    let message_id = message["messageId"].as_str().map(String::from).unwrap_or_else(|| ulid::Ulid::generate().to_string());
    // The conversation so far, if this caller started it.
    let history: Vec<ChatHistoryItem> = {
        let mut store = store().lock().unwrap();
        store.prune();
        match store.contexts.get(&context_id) {
            Some(c) if c.owner == principal.user_id => c.history.iter().map(|h| ChatHistoryItem { role: h.role.clone(), content: h.content.clone() }).collect(),
            Some(_) => return Ok(Err(error(id, INVALID_PARAMS, "That contextId belongs to someone else; start a new context."))),
            None => Vec::new(),
        }
    };
    let task_id = ulid::Ulid::generate().to_string();
    let user_message = json!({ "messageId": message_id, "role": "ROLE_USER", "parts": [{ "text": question }], "contextId": context_id, "taskId": task_id });
    Ok(Ok(Prepared { question, history, context_id, task_id, user_message, history_length: params["configuration"]["historyLength"].as_u64() }))
}

/// Keep a task (A2A 1.0 form) for GetTask and ListTasks; a finished task
/// goes to its webhooks.
fn keep(principal: &Principal, task: &Value) {
    let id = task["id"].as_str().unwrap_or_default().to_string();
    let configs = {
        let mut store = store().lock().unwrap();
        store.tasks.insert(id.clone(), StoredTask { owner: principal.user_id.clone(), task: task.clone(), at: Instant::now() });
        if finished(task) { store.push.get(&id).cloned().unwrap_or_default() } else { Vec::new() }
    };
    if !configs.is_empty() {
        a2a_push::notify(configs, task.clone());
    }
}

fn finished(task: &Value) -> bool {
    !matches!(task["status"]["state"].as_str(), Some("TASK_STATE_WORKING" | "TASK_STATE_SUBMITTED"))
}

/// The task a push config request names: 1.0 `taskId` (or `parent`
/// `tasks/{id}`), 0.3 `id` (`taskId` for set).
fn push_task_id(params: &Value, dialect: Dialect) -> String {
    let named = match dialect {
        Dialect::V1 => params["taskId"].as_str().or_else(|| params["parent"].as_str().and_then(|p| p.strip_prefix("tasks/"))),
        Dialect::V03 => params["taskId"].as_str().or_else(|| params["id"].as_str()),
    };
    named.unwrap_or_default().to_string()
}

/// Check a webhook and add it to a task (the task must be the caller's).
async fn add_push_config(engine: &hexdb_core::engine::HexDBEngine, principal: &Principal, task_id: &str, config: &Value, v03: bool) -> Result<PushConfig, (i64, String)> {
    let private_networks = engine.config_value("a2a.push_private_networks") == json!(true);
    let config = a2a_push::parse(config, v03, private_networks).map_err(|e| (INVALID_PARAMS, e))?;
    a2a_push::check(&config.url, private_networks).await.map_err(|e| (INVALID_PARAMS, e))?;
    let mut store = store().lock().unwrap();
    store.prune();
    if !store.tasks.get(task_id).is_some_and(|t| t.owner == principal.user_id) {
        return Err((TASK_NOT_FOUND, "No such task (tasks are kept for an hour).".into()));
    }
    let list = store.push.entry(task_id.to_string()).or_default();
    list.retain(|c| c.id != config.id);
    if list.len() >= MAX_PUSH_CONFIGS {
        return Err((INVALID_PARAMS, format!("A task can have at most {} push notification configs.", MAX_PUSH_CONFIGS)));
    }
    list.push(config.clone());
    Ok(config)
}

async fn create_push_config(engine: &hexdb_core::engine::HexDBEngine, principal: &Principal, id: &Value, params: &Value, dialect: Dialect) -> Value {
    let task_id = push_task_id(params, dialect);
    let config = if params["config"].is_object() { &params["config"] } else { params };
    match add_push_config(engine, principal, &task_id, config, dialect == Dialect::V03).await {
        Ok(config) => {
            // Already finished (it may have, before the client asked): send it now.
            if let Some(task) = owned_task(principal, &task_id).filter(finished) {
                a2a_push::notify(vec![config.clone()], task);
            }
            reply(id, a2a_push::view(&config, &task_id))
        }
        Err((code, message)) => error(id, code, &message),
    }
}

/// A task's webhooks, if the task is the caller's.
fn push_configs(principal: &Principal, task_id: &str) -> Option<Vec<PushConfig>> {
    let mut store = store().lock().unwrap();
    store.prune();
    let owned = store.tasks.get(task_id).is_some_and(|t| t.owner == principal.user_id);
    owned.then(|| store.push.get(task_id).cloned().unwrap_or_default())
}

/// The config ID a get or delete names (0.3: `pushNotificationConfigId`).
fn push_config_id(params: &Value, dialect: Dialect) -> Option<&str> {
    match dialect {
        Dialect::V1 => params["id"].as_str().or_else(|| params["name"].as_str().and_then(|n| n.rsplit('/').next())),
        Dialect::V03 => params["pushNotificationConfigId"].as_str(),
    }
}

fn get_push_config(principal: &Principal, id: &Value, params: &Value, dialect: Dialect) -> Value {
    let task_id = push_task_id(params, dialect);
    let Some(configs) = push_configs(principal, &task_id) else { return error(id, TASK_NOT_FOUND, "No such task.") };
    let wanted = push_config_id(params, dialect);
    match configs.iter().find(|c| wanted.is_none_or(|w| c.id == w)) {
        Some(config) => reply(id, a2a_push::view(config, &task_id)),
        None => error(id, INVALID_PARAMS, "No such push notification config."),
    }
}

fn list_push_configs(principal: &Principal, id: &Value, params: &Value, dialect: Dialect) -> Value {
    let task_id = push_task_id(params, dialect);
    let Some(configs) = push_configs(principal, &task_id) else { return error(id, TASK_NOT_FOUND, "No such task.") };
    let views: Vec<Value> = configs.iter().map(|c| a2a_push::view(c, &task_id)).collect();
    match dialect {
        Dialect::V1 => reply(id, json!({ "configs": views, "nextPageToken": "" })),
        Dialect::V03 => reply(id, json!(views)),
    }
}

fn delete_push_config(principal: &Principal, id: &Value, params: &Value, dialect: Dialect) -> Value {
    let task_id = push_task_id(params, dialect);
    let Some(wanted) = push_config_id(params, dialect).map(String::from) else { return error(id, INVALID_PARAMS, "Name the config to delete.") };
    if push_configs(principal, &task_id).is_none() {
        return error(id, TASK_NOT_FOUND, "No such task.");
    }
    let removed = store().lock().unwrap().push.get_mut(&task_id).is_some_and(|list| {
        let before = list.len();
        list.retain(|c| c.id != wanted);
        list.len() < before
    });
    match (removed, dialect) {
        (false, _) => error(id, INVALID_PARAMS, "No such push notification config."),
        (true, Dialect::V1) => reply(id, json!({})),
        (true, Dialect::V03) => reply(id, Value::Null),
    }
}

/// A task refused for the caller's own fault (permissions, input).
fn rejected(p: &Prepared, message: &str) -> Value {
    let reply = agent_message(message, &p.context_id, &p.task_id);
    json!({ "id": p.task_id, "contextId": p.context_id, "status": { "state": "TASK_STATE_REJECTED", "message": reply, "timestamp": now() }, "history": [p.user_message, reply] })
}

fn agent_message(text: &str, context_id: &str, task_id: &str) -> Value {
    json!({ "messageId": ulid::Ulid::generate().to_string(), "role": "ROLE_AGENT", "parts": [{ "text": text }], "contextId": context_id, "taskId": task_id })
}

/// Answer a prepared question and keep the finished task. Err only for the
/// request's own faults (permissions, input); provider problems and limits fail the task.
async fn run_task(
    engine: &std::sync::Arc<hexdb_core::engine::HexDBEngine>,
    principal: &Principal,
    client: &str,
    p: &Prepared,
    progress: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
) -> Result<Value, ApiError> {
    let (state, text, metadata) = match agent::answer(engine, principal, client, &p.question, &p.history, "a2a.message", progress).await {
        Ok(answer) => {
            let mut store = store().lock().unwrap();
            let context = store.contexts.entry(p.context_id.clone()).or_insert_with(|| Context { owner: principal.user_id.clone(), history: Vec::new(), at: Instant::now() });
            context.history.push(ChatHistoryItem { role: "user".into(), content: p.question.clone() });
            context.history.push(ChatHistoryItem { role: "assistant".into(), content: answer.answer.clone() });
            context.at = Instant::now();
            let metadata = json!({ "hexdb": { "model": answer.model, "tokens": { "input": answer.input_tokens, "output": answer.output_tokens }, "lookups": answer.steps.len() } });
            ("TASK_STATE_COMPLETED", answer.answer, metadata)
        }
        Err(e) if matches!(e.status(), StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED | StatusCode::BAD_REQUEST) => return Err(e),
        Err(e) => ("TASK_STATE_FAILED", e.message().to_string(), json!({ "hexdb": { "error": e.code() } })),
    };
    let reply = agent_message(&text, &p.context_id, &p.task_id);
    let mut task = json!({
        "id": p.task_id,
        "contextId": p.context_id,
        "status": { "state": state, "message": reply, "timestamp": now() },
        "history": [p.user_message, reply],
        "metadata": metadata,
    });
    if state == "TASK_STATE_COMPLETED" {
        task["artifacts"] = json!([{ "artifactId": ulid::Ulid::generate().to_string(), "name": "answer", "parts": [{ "text": text }] }]);
    }
    keep(principal, &task);
    Ok(task)
}

async fn send_message(engine: &std::sync::Arc<hexdb_core::engine::HexDBEngine>, principal: &Principal, client: &str, id: &Value, params: &Value, dialect: Dialect) -> Result<Value, ApiError> {
    let p = match prepare(engine, principal, id, params)? {
        Ok(p) => p,
        Err(reply) => return Ok(reply),
    };
    let working = json!({
        "id": p.task_id,
        "contextId": p.context_id,
        "status": { "state": "TASK_STATE_WORKING", "timestamp": now() },
        "history": [p.user_message],
    });
    keep(principal, &working);
    // A webhook for this task (1.0 taskPushNotificationConfig, 0.3 pushNotificationConfig).
    let configuration = &params["configuration"];
    let webhook = [&configuration["taskPushNotificationConfig"], &configuration["pushNotificationConfig"]].into_iter().find(|c| c.is_object());
    if let Some(webhook) = webhook {
        if let Err((code, message)) = add_push_config(engine, principal, &p.task_id, webhook, dialect == Dialect::V03).await {
            store().lock().unwrap().tasks.remove(&p.task_id);
            return Ok(error(id, code, &message));
        }
    }
    // Answer now with the working task, and finish it in the background (1.0
    // returnImmediately, 0.3 blocking: false); the webhook or GetTask has the result.
    if configuration["returnImmediately"] == json!(true) || configuration["blocking"] == json!(false) {
        let (engine, principal, client) = (engine.clone(), principal.clone(), client.to_string());
        let caller = std::sync::Arc::new(principal.clone());
        tokio::spawn(hexdb_core::access::as_caller(caller, async move {
            if let Err(e) = run_task(&engine, &principal, &client, &p, None).await {
                keep(&principal, &rejected(&p, e.message()));
            }
        }));
        return Ok(reply(id, shaped(&working, None, dialect)));
    }
    let result = run_task(engine, principal, client, &p, None).await;
    if let Err(e) = &result {
        keep(principal, &rejected(&p, e.message()));
    }
    match result {
        Ok(task) => Ok(reply(id, shaped(&task, p.history_length, dialect))),
        Err(e) if e.status() == StatusCode::BAD_REQUEST => Ok(error(id, INVALID_PARAMS, e.message())),
        Err(e) => Err(e),
    }
}

/// SendStreamingMessage (0.3: message/stream): Server-Sent Events, each a
/// JSON-RPC response whose result is a StreamResponse: the task (working),
/// a status update per lookup the chat makes, the answer as an artifact
/// update, and the final status. The answer runs in its own task, as the
/// caller, and is kept for GetTask even if the client goes away.
async fn send_streaming(
    engine: std::sync::Arc<hexdb_core::engine::HexDBEngine>,
    principal: Principal,
    client: String,
    id: Value,
    params: Value,
    dialect: Dialect,
) -> ApiResult {
    use axum::response::sse::{Event, KeepAlive, Sse};
    let p = match prepare(&engine, &principal, &id, &params)? {
        Ok(p) => p,
        Err(reply) => return Ok(Json(reply).into_response()),
    };
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let started = json!({
        "id": p.task_id,
        "contextId": p.context_id,
        "status": { "state": "TASK_STATE_WORKING", "timestamp": now() },
        "history": [p.user_message],
    });
    keep(&principal, &started);
    let _ = events.send(json!({ "task": started }));
    let caller = std::sync::Arc::new(principal.clone());
    tokio::spawn(hexdb_core::access::as_caller(caller, async move {
        let (progress, mut steps) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (task_id, context_id) = (p.task_id.clone(), p.context_id.clone());
        let relay = {
            let events = events.clone();
            let (task_id, context_id) = (task_id.clone(), context_id.clone());
            tokio::spawn(async move {
                while let Some(step) = steps.recv().await {
                    let status = json!({ "state": "TASK_STATE_WORKING", "message": agent_message(&step, &context_id, &task_id), "timestamp": now() });
                    let _ = events.send(json!({ "statusUpdate": { "taskId": task_id, "contextId": context_id, "status": status } }));
                }
            })
        };
        let task = match run_task(&engine, &principal, &client, &p, Some(&progress)).await {
            Ok(task) => task,
            Err(e) => {
                let task = rejected(&p, e.message());
                keep(&principal, &task);
                task
            }
        };
        drop(progress);
        let _ = relay.await;
        if let Some(artifact) = task["artifacts"].get(0) {
            let _ = events.send(json!({ "artifactUpdate": { "taskId": task_id, "contextId": context_id, "artifact": artifact, "append": false, "lastChunk": true } }));
        }
        let _ = events.send(json!({ "statusUpdate": { "taskId": task_id, "contextId": context_id, "status": task["status"], "metadata": task["metadata"] } }));
    }));
    use futures::StreamExt;
    let stream = futures::stream::unfold(receiver, |mut r| async move { r.recv().await.map(|v| (v, r)) }).map(move |result| {
        let result = if dialect == Dialect::V03 { stream_v03(result) } else { result };
        Ok::<_, std::convert::Infallible>(Event::default().data(reply(&id, result).to_string()))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()).into_response())
}

/// A 1.0 StreamResponse in 0.3's form: the bare object with its `kind`
/// (task, status-update with `final`, artifact-update).
fn stream_v03(result: Value) -> Value {
    if let Some(task) = result.get("task") {
        return to_v03(task.clone());
    }
    if let Some(update) = result.get("statusUpdate") {
        let mut update = update.clone();
        let state = update["status"]["state"].as_str().unwrap_or_default().to_string();
        update["kind"] = json!("status-update");
        update["final"] = json!(state != "TASK_STATE_WORKING");
        update["status"]["state"] = json!(state.trim_start_matches("TASK_STATE_").to_ascii_lowercase());
        if update["status"]["message"].is_object() {
            let message = to_v03(json!({ "history": [update["status"]["message"]] }))["history"][0].clone();
            update["status"]["message"] = message;
        }
        return update;
    }
    if let Some(update) = result.get("artifactUpdate") {
        let mut update = update.clone();
        update["kind"] = json!("artifact-update");
        for part in update["artifact"]["parts"].as_array_mut().into_iter().flatten() {
            part["kind"] = json!("text");
        }
        return update;
    }
    result
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

/// A task in 0.3's form (also what 0.3 webhooks receive).
pub(crate) fn to_v03(mut task: Value) -> Value {
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
