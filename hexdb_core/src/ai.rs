// HexDB Core AI
//
// One client for the AI provider set in the lattice configuration (`ai.*`,
// see `crate::lattice_config`): Anthropic (Claude) through its Messages API,
// and OpenAI, GitHub Models, Azure OpenAI and any OpenAI-compatible server
// (Ollama, LM Studio, vLLM) through chat completions. Every AI feature (the
// query advisor, file tagging, video thumbnails, plugins through `POST
// /ai/complete`) goes through `HexDBEngine::ai_complete`, so:
//
//   * calls only ever use the key this lattice's administrators entered;
//   * nothing is sent while `ai.provider` is `none` (the default);
//   * the tokens each call used are added to the month's usage, per feature,
//     in the `_ai_usage` system tessellation (replicated; replicas send their
//     usage to the Overseer), and calls stop once `ai.monthly_token_budget`
//     is reached;
//   * the key is never logged, and provider errors are passed on without it.
//
// Before the lattice configuration existed, the query advisor read an
// Anthropic key from the environment variable named by `[ai] api_key_env`.
// That still works while `ai.provider` is `none`, for the advisor only.

use crate::engine::{EngineError, HexDBEngine};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{Datelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::time::{Duration, Instant};
use tracing::warn;
use ulid::Ulid;

/// Monthly usage, one document per month.
pub const AI_USAGE_TESSELLATION: &str = "_ai_usage";
const ANTHROPIC_API: &str = "https://api.anthropic.com";
const OPENAI_API: &str = "https://api.openai.com/v1";
const GITHUB_MODELS_API: &str = "https://models.github.ai/inference";
/// The default Claude model.
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-opus-5-5";
/// Claude models that accept server-side refusal fallbacks (`fallbacks: "default"`).
const FALLBACK_MODELS: &[&str] = &["claude-fable-5-1", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

/// Part of a prompt.
#[derive(Debug, Clone)]
pub enum Part {
    Text(String),
    /// An image: its MIME type and bytes.
    Image { media_type: String, data: Vec<u8> },
    /// A whole PDF, read page by page (text and pictures) by providers that
    /// take PDFs (see [`reads_pdfs`]).
    Pdf { data: Vec<u8> },
}

/// True if a provider's API takes whole PDFs: Anthropic's document blocks and
/// OpenAI's file parts. Other servers get only what was extracted.
pub fn reads_pdfs(provider: &str) -> bool {
    matches!(provider, "anthropic" | "openai")
}

/// One prompt: instructions and the content to work on.
#[derive(Debug, Clone)]
pub struct AiRequest {
    /// Which feature asks (usage is kept per feature).
    pub feature: String,
    pub system: String,
    pub parts: Vec<Part>,
    /// Most tokens the answer may use (capped by `ai.max_tokens_per_file`).
    pub max_tokens: u64,
}

/// An answer, with what it cost.
#[derive(Debug, Clone, Serialize)]
pub struct AiReply {
    pub text: String,
    pub model: String,
    pub provider: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub millis: u64,
}

/// A tool the model may call in a conversation (`ai_chat`).
#[derive(Debug, Clone, Serialize)]
pub struct AiTool {
    pub name: String,
    pub description: String,
    /// JSON Schema of the tool's input.
    pub input_schema: Value,
}

/// A call the model made to a tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// What a tool returned, sent back to the model.
#[derive(Debug, Clone, Serialize)]
pub struct ToolResult {
    pub id: String,
    pub content: String,
    pub is_error: bool,
}

/// One step of a conversation.
#[derive(Debug, Clone)]
pub enum Turn {
    User(String),
    Assistant { text: String, calls: Vec<ToolCall> },
    Results(Vec<ToolResult>),
}

/// A conversation with tools.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// Which feature asks (usage is kept per feature).
    pub feature: String,
    pub system: String,
    pub turns: Vec<Turn>,
    pub tools: Vec<AiTool>,
    pub max_tokens: u64,
    /// A model to use instead of `ai.model` (e.g. a smaller one for chat).
    pub model: Option<String>,
}

/// The model's next step: text, tool calls, or both.
#[derive(Debug, Clone, Serialize)]
pub struct ChatReply {
    pub text: String,
    pub calls: Vec<ToolCall>,
    pub model: String,
    pub provider: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub millis: u64,
}

/// Where calls go, from the lattice configuration.
#[derive(Clone)]
struct Provider {
    kind: String,
    model: String,
    /// Full base URL, without a trailing slash.
    base_url: String,
    key: Option<crate::lattice_config::Secret>,
    /// Using the pre-1.6 `[ai]` environment key (advisor only).
    legacy: bool,
}

impl HexDBEngine {
    fn ai_provider(&self, feature: &str) -> Result<Provider> {
        let text = |key: &str| self.config_value(key).as_str().unwrap_or_default().trim().to_string();
        let kind = text("ai.provider");
        if kind == "none" || kind.is_empty() {
            // The advisor's pre-1.6 setting: an Anthropic key in the environment.
            let legacy = &self.config.ai;
            if feature == "advisor" {
                if let Ok(key) = std::env::var(&legacy.api_key_env) {
                    if !key.trim().is_empty() {
                        return Ok(Provider {
                            kind: "anthropic".into(),
                            model: legacy.model.clone(),
                            base_url: legacy.base_url.trim_end_matches('/').to_string(),
                            key: Some(crate::lattice_config::Secret::from_env(key.trim())),
                            legacy: true,
                        });
                    }
                }
            }
            return Err(EngineError::Invalid("No AI provider is configured: set ai.provider and ai.api_key on the Configuration page or with `hexdb config`.".into()).into());
        }
        let configured_url = text("ai.base_url");
        let base_url = match (kind.as_str(), configured_url.is_empty()) {
            (_, false) => configured_url,
            ("anthropic", true) => ANTHROPIC_API.into(),
            ("openai", true) => OPENAI_API.into(),
            ("github", true) => GITHUB_MODELS_API.into(),
            (_, true) => return Err(EngineError::Invalid(format!("ai.base_url is required for the {} provider.", kind)).into()),
        };
        let mut model = text("ai.model");
        if model.is_empty() {
            if kind != "anthropic" {
                return Err(EngineError::Invalid(format!("Set ai.model: the {} provider has no default model in HexDB.", kind)).into());
            }
            model = DEFAULT_CLAUDE_MODEL.into();
        }
        let key = self.config_secret("ai.api_key");
        if key.is_none() && kind != "openai_compatible" {
            return Err(EngineError::Invalid("Set ai.api_key for the AI provider.".into()).into());
        }
        Ok(Provider { kind, model, base_url: base_url.trim_end_matches('/').to_string(), key, legacy: false })
    }

    /// The configured provider and model, for display (no key).
    pub fn ai_summary(&self) -> Value {
        match self.ai_provider("summary") {
            Ok(p) => json!({ "configured": true, "provider": p.kind, "model": p.model, "base_url": p.base_url }),
            Err(e) => json!({ "configured": false, "provider": self.config_value("ai.provider"), "problem": e.to_string() }),
        }
    }

    /// Run one prompt through the lattice's AI provider, within the budget,
    /// and record the tokens it used.
    pub async fn ai_complete(&self, request: AiRequest) -> Result<AiReply> {
        let provider = self.ai_provider(&request.feature)?;
        if !provider.legacy {
            self.check_ai_budget().await?;
        }
        // File features stay within ai.max_tokens_per_file; others within 16,000.
        let cap = if request.feature.starts_with("files.") {
            self.config_value("ai.max_tokens_per_file").as_u64().unwrap_or(4000).max(100)
        } else {
            16_000
        };
        let max_tokens = request.max_tokens.clamp(16, cap);
        let started = Instant::now();
        let result = match provider.kind.as_str() {
            "anthropic" => call_anthropic(&provider, &request, max_tokens).await,
            _ => call_chat_completions(&provider, &request, max_tokens).await,
        };
        let millis = started.elapsed().as_millis() as u64;
        match result {
            Ok((text, input_tokens, output_tokens)) => {
                self.record_ai_usage(&request.feature, input_tokens, output_tokens, true).await;
                Ok(AiReply { text, model: provider.model, provider: provider.kind, input_tokens, output_tokens, millis })
            }
            Err(e) => {
                self.record_ai_usage(&request.feature, 0, 0, false).await;
                Err(e)
            }
        }
    }

    /// One step of a conversation with tools, within the budget; the tokens
    /// it used are recorded under `request.feature`.
    pub async fn ai_chat(&self, request: ChatRequest) -> Result<ChatReply> {
        let mut provider = self.ai_provider(&request.feature)?;
        if let Some(model) = request.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
            provider.model = model.to_string();
        }
        self.check_ai_budget().await?;
        let max_tokens = request.max_tokens.clamp(16, 16_000);
        let started = Instant::now();
        let result = match provider.kind.as_str() {
            "anthropic" => chat_anthropic(&provider, &request, max_tokens).await,
            _ => chat_completions_with_tools(&provider, &request, max_tokens).await,
        };
        let millis = started.elapsed().as_millis() as u64;
        match result {
            Ok((text, calls, input_tokens, output_tokens)) => {
                self.record_ai_usage(&request.feature, input_tokens, output_tokens, true).await;
                Ok(ChatReply { text, calls, model: provider.model, provider: provider.kind, input_tokens, output_tokens, millis })
            }
            Err(e) => {
                self.record_ai_usage(&request.feature, 0, 0, false).await;
                Err(e)
            }
        }
    }

    /// A tiny call that shows whether the provider, model and key work.
    pub async fn ai_test(&self) -> Result<AiReply> {
        self.ai_complete(AiRequest {
            feature: "test".into(),
            system: "You check that an API connection works.".into(),
            parts: vec![Part::Text("Reply with the single word OK.".into())],
            max_tokens: 2000,
        })
        .await
    }

    /// Refuse when this month's usage has reached `ai.monthly_token_budget`.
    async fn check_ai_budget(&self) -> Result<()> {
        let budget = self.config_value("ai.monthly_token_budget").as_u64().unwrap_or(0);
        if budget == 0 {
            return Ok(());
        }
        let used = self.ai_month_usage().await?;
        let total = used["input_tokens"].as_u64().unwrap_or(0) + used["output_tokens"].as_u64().unwrap_or(0);
        if total >= budget {
            let (resets, seconds) = next_month_utc();
            return Err(EngineError::RateLimited(
                format!("This month's AI token budget ({} tokens) is used up. It resets on {} (UTC); an administrator can also raise ai.monthly_token_budget.", budget, resets),
                seconds,
            )
            .into());
        }
        Ok(())
    }

    /// This month's usage document (zeros when there is none).
    pub async fn ai_month_usage(&self) -> Result<Value> {
        let (id, month) = month_key(Utc::now());
        let stored = self.get_system_document(AI_USAGE_TESSELLATION, id).await?.map(|d| d.data_json());
        Ok(stored.unwrap_or_else(|| json!({ "month": month, "input_tokens": 0, "output_tokens": 0, "calls": 0, "failures": 0, "features": {} })))
    }

    /// Usage this month and the budget, for display.
    pub async fn ai_usage_view(&self) -> Result<Value> {
        let mut usage = self.ai_month_usage().await?;
        let budget = self.config_value("ai.monthly_token_budget").as_u64().unwrap_or(0);
        let total = usage["input_tokens"].as_u64().unwrap_or(0) + usage["output_tokens"].as_u64().unwrap_or(0);
        usage["total_tokens"] = json!(total);
        usage["budget"] = json!(budget);
        usage["remaining"] = if budget == 0 { Value::Null } else { json!(budget.saturating_sub(total)) };
        Ok(usage)
    }

    /// Add a call to the month's usage: stored on the Overseer, sent there from a replica.
    pub async fn record_ai_usage(&self, feature: &str, input_tokens: u64, output_tokens: u64, ok: bool) {
        let entry = UsageEntry { feature: feature.to_string(), input_tokens, output_tokens, ok, time: Utc::now().timestamp_millis() };
        let outcome = if self.is_writable() {
            self.store_ai_usage(&entry).await
        } else {
            match serde_json::to_value(&entry) {
                Ok(body) => tokio::time::timeout(Duration::from_secs(3), crate::replication::post_to_overseer(self, "/lattice/ai-usage", &body))
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("timed out"))),
                Err(e) => Err(e.into()),
            }
        };
        if let Err(e) = outcome {
            warn!("⚠️ Couldn't record AI usage ({}: {} in, {} out): {:#}", feature, input_tokens, output_tokens, e);
        }
    }

    /// Store a usage entry (the Overseer; entries from replicas come through `/lattice/ai-usage`).
    pub async fn store_ai_usage(&self, entry: &UsageEntry) -> Result<()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _guard = LOCK.lock().await;
        let when = chrono::DateTime::from_timestamp_millis(entry.time).unwrap_or_else(Utc::now);
        let (id, month) = month_key(when);
        let mut doc = self
            .get_system_document(AI_USAGE_TESSELLATION, id)
            .await?
            .map(|d| d.data_json())
            .unwrap_or_else(|| json!({ "month": month, "input_tokens": 0, "output_tokens": 0, "calls": 0, "failures": 0, "features": {} }));
        let add = |v: &mut Value, n: u64| *v = json!(v.as_u64().unwrap_or(0) + n);
        add(&mut doc["input_tokens"], entry.input_tokens);
        add(&mut doc["output_tokens"], entry.output_tokens);
        add(&mut doc[if entry.ok { "calls" } else { "failures" }], 1);
        if !doc["features"].is_object() {
            doc["features"] = json!({});
        }
        let feature = doc["features"].as_object_mut().expect("an object").entry(entry.feature.clone()).or_insert_with(|| json!({}));
        add(&mut feature["input_tokens"], entry.input_tokens);
        add(&mut feature["output_tokens"], entry.output_tokens);
        add(&mut feature[if entry.ok { "calls" } else { "failures" }], 1);
        doc.as_object_mut().expect("an object").remove("id");
        self.put_system_document(AI_USAGE_TESSELLATION, id, doc, None).await
    }
}

/// One call's usage, as sent from a replica to the Overseer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageEntry {
    pub feature: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub ok: bool,
    pub time: i64,
}

/// The usage document's ID and label for a month.
fn month_key(when: chrono::DateTime<Utc>) -> (Ulid, String) {
    let (year, month) = (when.year() as u128, when.month() as u128);
    (Ulid::from(year * 100 + month), format!("{:04}-{:02}", year, month))
}

fn client() -> Result<reqwest::Client> {
    Ok(crate::crypt::http_client().timeout(REQUEST_TIMEOUT).connect_timeout(Duration::from_secs(15)).build()?)
}

/// A provider's error without anything that could carry the key, shortened.
fn provider_error(provider: &str, status: reqwest::StatusCode, body: &str, key: Option<&str>) -> anyhow::Error {
    let mut text: String = body.chars().take(500).collect();
    if let Some(key) = key.filter(|k| k.len() >= 8) {
        text = text.replace(key, "[key]");
    }
    let message = format!("The {} API returned {}: {}", provider, status, text.trim());
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        EngineError::RateLimited(message, 30).into()
    } else {
        anyhow!(message)
    }
}

/// POST a Messages API body to Claude; the parsed answer and its token counts.
async fn post_anthropic(provider: &Provider, mut body: Value) -> Result<(Value, u64, u64)> {
    let official = provider.base_url == ANTHROPIC_API;
    let fallbacks = official && FALLBACK_MODELS.contains(&provider.model.as_str());
    if fallbacks {
        // A request the model declines is retried on another model, chosen by the API.
        body["fallbacks"] = json!("default");
    }
    let key = provider.key.as_ref().map(|k| k.expose().to_string()).unwrap_or_default();
    let mut call = client()?.post(format!("{}/v1/messages", provider.base_url)).header("x-api-key", &key).header("anthropic-version", "2023-06-01").json(&body);
    if fallbacks {
        call = call.header("anthropic-beta", "server-side-fallback-2026-07-01");
    }
    let response = call.send().await.context("calling the Claude API")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(provider_error("Claude", status, &text, Some(&key)));
    }
    let reply: Value = serde_json::from_str(&text).context("the Claude API's answer wasn't JSON")?;
    let usage = &reply["usage"];
    let input = usage["input_tokens"].as_u64().unwrap_or(0)
        + usage["cache_creation_input_tokens"].as_u64().unwrap_or(0)
        + usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
    let output = usage["output_tokens"].as_u64().unwrap_or(0);
    Ok((reply, input, output))
}

/// A conversation with tools through the Messages API.
async fn chat_anthropic(provider: &Provider, request: &ChatRequest, max_tokens: u64) -> Result<(String, Vec<ToolCall>, u64, u64)> {
    let messages: Vec<Value> = request
        .turns
        .iter()
        .map(|turn| match turn {
            Turn::User(text) => json!({ "role": "user", "content": [{ "type": "text", "text": text }] }),
            Turn::Assistant { text, calls } => {
                let mut content: Vec<Value> = Vec::new();
                if !text.is_empty() {
                    content.push(json!({ "type": "text", "text": text }));
                }
                content.extend(calls.iter().map(|c| json!({ "type": "tool_use", "id": c.id, "name": c.name, "input": c.input })));
                json!({ "role": "assistant", "content": content })
            }
            Turn::Results(results) => json!({
                "role": "user",
                "content": results.iter().map(|r| json!({ "type": "tool_result", "tool_use_id": r.id, "content": r.content, "is_error": r.is_error })).collect::<Vec<_>>(),
            }),
        })
        .collect();
    let tools: Vec<Value> = request.tools.iter().map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.input_schema })).collect();
    let mut body = json!({ "model": provider.model, "max_tokens": max_tokens, "system": request.system, "messages": messages });
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    let (reply, input, output) = post_anthropic(provider, body).await?;
    match reply["stop_reason"].as_str() {
        Some("refusal") => bail!("The model declined the request ({}).", reply["stop_details"]["category"].as_str().unwrap_or("no category")),
        Some("max_tokens") => bail!("The answer didn't fit in {} tokens.", max_tokens),
        _ => {}
    }
    let blocks = reply["content"].as_array().cloned().unwrap_or_default();
    let text: String = blocks.iter().filter(|c| c["type"] == "text").filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("");
    let calls = blocks
        .iter()
        .filter(|c| c["type"] == "tool_use")
        .map(|c| ToolCall { id: c["id"].as_str().unwrap_or_default().to_string(), name: c["name"].as_str().unwrap_or_default().to_string(), input: c["input"].clone() })
        .collect();
    Ok((text, calls, input, output))
}

async fn call_anthropic(provider: &Provider, request: &AiRequest, max_tokens: u64) -> Result<(String, u64, u64)> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let content: Vec<Value> = request
        .parts
        .iter()
        .map(|p| match p {
            Part::Text(text) => json!({ "type": "text", "text": text }),
            Part::Image { media_type, data } => json!({ "type": "image", "source": { "type": "base64", "media_type": media_type, "data": STANDARD.encode(data) } }),
            Part::Pdf { data } => json!({ "type": "document", "source": { "type": "base64", "media_type": "application/pdf", "data": STANDARD.encode(data) } }),
        })
        .collect();
    let body = json!({
        "model": provider.model,
        "max_tokens": max_tokens,
        "system": request.system,
        "messages": [{ "role": "user", "content": content }],
    });
    let (reply, input, output) = post_anthropic(provider, body).await?;
    match reply["stop_reason"].as_str() {
        Some("refusal") => bail!("The model declined the request ({}).", reply["stop_details"]["category"].as_str().unwrap_or("no category")),
        Some("max_tokens") => bail!("The answer didn't fit in {} tokens; raise ai.max_tokens_per_file.", max_tokens),
        _ => {}
    }
    let answer: Vec<&str> = reply["content"].as_array().into_iter().flatten().filter(|c| c["type"] == "text").filter_map(|c| c["text"].as_str()).collect();
    Ok((answer.join(""), input, output))
}

fn provider_name(kind: &str) -> &'static str {
    match kind {
        "openai" => "OpenAI",
        "github" => "GitHub Models",
        "azure_openai" => "Azure OpenAI",
        _ => "AI",
    }
}

/// POST a chat completions body; the parsed answer and its token counts.
async fn post_chat_completions(provider: &Provider, mut body: Map<String, Value>, max_tokens: u64) -> Result<(Value, u64, u64)> {
    // OpenAI's own APIs take max_completion_tokens; other servers still take max_tokens.
    let limit_name = if matches!(provider.kind.as_str(), "openai" | "azure_openai") { "max_completion_tokens" } else { "max_tokens" };
    body.insert(limit_name.into(), json!(max_tokens));
    let key = provider.key.as_ref().map(|k| k.expose().to_string()).unwrap_or_default();
    let mut call = client()?.post(format!("{}/chat/completions", provider.base_url)).json(&Value::Object(body));
    if !key.is_empty() {
        call = if provider.kind == "azure_openai" { call.header("api-key", &key) } else { call.bearer_auth(&key) };
    }
    let name = provider_name(&provider.kind);
    let response = call.send().await.with_context(|| format!("calling the {} API", name))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(provider_error(name, status, &text, Some(&key)));
    }
    let reply: Value = serde_json::from_str(&text).with_context(|| format!("the {} API's answer wasn't JSON", name))?;
    let input = reply["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
    let output = reply["usage"]["completion_tokens"].as_u64().unwrap_or(0);
    Ok((reply, input, output))
}

/// A conversation with tools through chat completions (function calling).
async fn chat_completions_with_tools(provider: &Provider, request: &ChatRequest, max_tokens: u64) -> Result<(String, Vec<ToolCall>, u64, u64)> {
    let mut messages = vec![json!({ "role": "system", "content": request.system })];
    for turn in &request.turns {
        match turn {
            Turn::User(text) => messages.push(json!({ "role": "user", "content": text })),
            Turn::Assistant { text, calls } => {
                let mut message = json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) } });
                if !calls.is_empty() {
                    message["tool_calls"] = json!(calls
                        .iter()
                        .map(|c| json!({ "id": c.id, "type": "function", "function": { "name": c.name, "arguments": c.input.to_string() } }))
                        .collect::<Vec<_>>());
                }
                messages.push(message);
            }
            Turn::Results(results) => {
                for r in results {
                    let content = if r.is_error { format!("Error: {}", r.content) } else { r.content.clone() };
                    messages.push(json!({ "role": "tool", "tool_call_id": r.id, "content": content }));
                }
            }
        }
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(provider.model));
    body.insert("messages".into(), json!(messages));
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request.tools.iter().map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.input_schema } })).collect();
        body.insert("tools".into(), json!(tools));
    }
    let (reply, input, output) = post_chat_completions(provider, body, max_tokens).await?;
    let choice = &reply["choices"][0];
    match choice["finish_reason"].as_str() {
        Some("content_filter") => bail!("The provider's content filter declined the request."),
        Some("length") => bail!("The answer didn't fit in {} tokens.", max_tokens),
        _ => {}
    }
    let message = &choice["message"];
    let text = match &message["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(""),
        _ => String::new(),
    };
    let calls = message["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| ToolCall {
            id: c["id"].as_str().unwrap_or_default().to_string(),
            name: c["function"]["name"].as_str().unwrap_or_default().to_string(),
            // Arguments arrive as a JSON string; a malformed one becomes an empty input the tool rejects.
            input: c["function"]["arguments"].as_str().and_then(|a| serde_json::from_str(a).ok()).unwrap_or_else(|| json!({})),
        })
        .collect();
    Ok((text, calls, input, output))
}

async fn call_chat_completions(provider: &Provider, request: &AiRequest, max_tokens: u64) -> Result<(String, u64, u64)> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let content: Vec<Value> = request
        .parts
        .iter()
        .map(|p| match p {
            Part::Text(text) => json!({ "type": "text", "text": text }),
            Part::Image { media_type, data } => {
                json!({ "type": "image_url", "image_url": { "url": format!("data:{};base64,{}", media_type, STANDARD.encode(data)) } })
            }
            Part::Pdf { data } => {
                json!({ "type": "file", "file": { "filename": "file.pdf", "file_data": format!("data:application/pdf;base64,{}", STANDARD.encode(data)) } })
            }
        })
        .collect();
    let mut body = Map::new();
    body.insert("model".into(), json!(provider.model));
    body.insert("messages".into(), json!([{ "role": "system", "content": request.system }, { "role": "user", "content": content }]));
    let (reply, input, output) = post_chat_completions(provider, body, max_tokens).await?;
    let choice = &reply["choices"][0];
    match choice["finish_reason"].as_str() {
        Some("content_filter") => bail!("The provider's content filter declined the request."),
        Some("length") => bail!("The answer didn't fit in {} tokens; raise ai.max_tokens_per_file.", max_tokens),
        _ => {}
    }
    let answer = match &choice["message"]["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(""),
        _ => String::new(),
    };
    Ok((answer, input, output))
}

/// When the monthly budget resets: the first of next month (UTC), as "November 1", and the seconds until then.
pub fn next_month_utc() -> (String, u64) {
    use chrono::{Datelike, TimeZone, Utc};
    let now = Utc::now();
    let (year, month) = if now.month() == 12 { (now.year() + 1, 1) } else { (now.year(), now.month() + 1) };
    let first = Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0).single().unwrap_or(now);
    (first.format("%B %-d").to_string(), (first - now).num_seconds().max(1) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months_have_stable_ids() {
        let a = chrono::DateTime::parse_from_rfc3339("2026-10-01T00:00:00Z").unwrap().with_timezone(&Utc);
        let b = chrono::DateTime::parse_from_rfc3339("2026-10-31T23:59:59Z").unwrap().with_timezone(&Utc);
        let c = chrono::DateTime::parse_from_rfc3339("2026-11-01T00:00:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(month_key(a), month_key(b));
        assert_ne!(month_key(a).0, month_key(c).0);
        assert_eq!(month_key(a).1, "2026-10");
    }

    #[test]
    fn provider_errors_never_carry_the_key() {
        let e = provider_error("Claude", reqwest::StatusCode::UNAUTHORIZED, "invalid x-api-key sk-ant-secret-123456", Some("sk-ant-secret-123456"));
        assert!(!e.to_string().contains("sk-ant-secret"));
    }
}
