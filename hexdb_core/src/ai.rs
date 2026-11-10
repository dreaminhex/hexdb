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
const VOYAGE_API: &str = "https://api.voyageai.com/v1";
/// Most texts in one embeddings request.
pub const EMBED_BATCH: usize = 64;
/// The default Claude model.
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-opus-5-5";
/// Claude models that accept server-side refusal fallbacks (`fallbacks: "default"`).
const FALLBACK_MODELS: &[&str] = &["claude-fable-5-1", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
/// How long a call waits for one of the hex's `ai.max_concurrent_calls` slots.
const SLOT_WAIT: Duration = Duration::from_secs(30);

/// The AI calls running on this hex, held to `ai.max_concurrent_calls`, so a
/// burst of questions, file tagging and embeddings can't send a provider more
/// at once than its rate limits (or a local model's machine) allow.
#[derive(Debug, Default)]
pub struct AiGate {
    running: std::sync::Mutex<usize>,
    freed: tokio::sync::Notify,
}

/// A running call's slot, given back when it's dropped.
pub struct AiSlot<'a>(&'a AiGate);

impl Drop for AiSlot<'_> {
    fn drop(&mut self) {
        *self.0.running.lock().unwrap() -= 1;
        self.0.freed.notify_one();
    }
}

impl AiGate {
    /// The calls running now.
    pub fn running(&self) -> usize {
        *self.running.lock().unwrap()
    }

    /// Wait for a slot (at most `limit` calls at once; 0: no limit), up to `wait`.
    pub async fn slot(&self, limit: usize, wait: Duration) -> Result<AiSlot<'_>> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            // Registered before checking, so a slot freed in between isn't missed.
            let freed = self.freed.notified();
            {
                let mut running = self.running.lock().unwrap();
                if limit == 0 || *running < limit {
                    *running += 1;
                    return Ok(AiSlot(self));
                }
            }
            if tokio::time::timeout_at(deadline, freed).await.is_err() {
                return Err(EngineError::RateLimited(
                    format!("The AI provider is busy: {} calls are already running on this hex (ai.max_concurrent_calls). Try again shortly.", limit),
                    5,
                )
                .into());
            }
        }
    }
}

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
    /// Set to stream: the reply's text is sent here piece by piece as the
    /// provider produces it (the whole reply is still returned at the end).
    pub deltas: Option<tokio::sync::mpsc::UnboundedSender<ChatStream>>,
}

/// What a streamed chat sends as it goes.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatStream {
    /// A piece of the model's text.
    Text(String),
    /// A lookup the model asked for (the text before it wasn't the answer).
    Lookup { tool: String, tessellation: Option<String> },
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

/// What a text is embedded for (Voyage embeds the two differently).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EmbedPurpose {
    Document,
    Query,
}

/// What one embedding is made of: text, and a picture for multimodal models.
#[derive(Debug, Clone, Default)]
pub struct EmbedInput {
    pub text: String,
    /// The picture's content type and bytes (a thumbnail).
    pub image: Option<(String, Vec<u8>)>,
}

/// True for an embeddings model that takes pictures (Voyage AI's multimodal models).
pub fn takes_images(kind: &str, model: &str) -> bool {
    kind == "voyage" && model.starts_with("voyage-multimodal")
}

/// Vectors for texts, in their order.
#[derive(Debug, Clone)]
pub struct Embeddings {
    pub vectors: Vec<Vec<f32>>,
    pub model: String,
    pub tokens: u64,
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

    /// Where embeddings come from (`ai.embeddings.*`).
    fn embedding_provider(&self) -> Result<Provider> {
        let text = |key: &str| self.config_value(key).as_str().unwrap_or_default().trim().to_string();
        let chosen = text("ai.embeddings.provider");
        let same = chosen.is_empty() || chosen == "same";
        let kind = if same { text("ai.provider") } else { chosen };
        let not_configured = |why: String| -> anyhow::Error { EngineError::Invalid(why).into() };
        match kind.as_str() {
            "" | "none" => return Err(not_configured("No embeddings provider is configured: set ai.embeddings.provider (or ai.provider).".into())),
            "anthropic" => {
                return Err(not_configured(
                    "Anthropic has no embeddings API: set ai.embeddings.provider to voyage (Anthropic's recommended embeddings) or another provider.".into(),
                ))
            }
            _ => {}
        }
        let mut base_url = text("ai.embeddings.base_url");
        if base_url.is_empty() && same {
            base_url = text("ai.base_url");
        }
        if base_url.is_empty() {
            base_url = match kind.as_str() {
                "openai" => OPENAI_API.into(),
                "voyage" => VOYAGE_API.into(),
                "github" => GITHUB_MODELS_API.into(),
                _ => return Err(not_configured(format!("ai.embeddings.base_url is required for the {} provider.", kind))),
            };
        }
        let mut model = text("ai.embeddings.model");
        if model.is_empty() {
            model = match kind.as_str() {
                "openai" => "text-embedding-3-small".into(),
                "github" => "openai/text-embedding-3-small".into(),
                "voyage" => "voyage-3.5".into(),
                _ => return Err(not_configured(format!("Set ai.embeddings.model: the {} provider has no default embedding model in HexDB.", kind))),
            };
        }
        let key = self.config_secret("ai.embeddings.api_key").or_else(|| if same { self.config_secret("ai.api_key") } else { None });
        if key.is_none() && kind != "openai_compatible" {
            return Err(not_configured("Set ai.embeddings.api_key for the embeddings provider.".into()));
        }
        Ok(Provider { kind, model, base_url: base_url.trim_end_matches('/').to_string(), key, legacy: false })
    }

    /// The embedding model in use, if embeddings are configured.
    pub fn embedding_model(&self) -> Option<String> {
        self.embedding_provider().ok().map(|p| p.model)
    }

    /// Embed texts with the embeddings provider, within the monthly budget;
    /// the tokens are recorded under `feature`.
    pub async fn ai_embed(&self, texts: &[String], purpose: EmbedPurpose, feature: &str) -> Result<Embeddings> {
        let inputs: Vec<EmbedInput> = texts.iter().map(|t| EmbedInput { text: t.clone(), image: None }).collect();
        self.ai_embed_inputs(&inputs, purpose, feature).await
    }

    /// [`ai_embed`](Self::ai_embed) for inputs that may have pictures: a
    /// multimodal model embeds each picture with its text, in the same space
    /// as text queries; other models refuse pictures.
    pub async fn ai_embed_inputs(&self, inputs: &[EmbedInput], purpose: EmbedPurpose, feature: &str) -> Result<Embeddings> {
        let provider = self.embedding_provider()?;
        if inputs.is_empty() {
            return Ok(Embeddings { vectors: Vec::new(), model: provider.model, tokens: 0 });
        }
        let multimodal = takes_images(&provider.kind, &provider.model);
        if !multimodal && inputs.iter().any(|i| i.image.is_some()) {
            bail!(
                "The embeddings model {} doesn't take pictures. To embed files' pictures ($image), use a multimodal model: ai.embeddings.provider voyage with ai.embeddings.model voyage-multimodal-3.",
                provider.model
            );
        }
        self.check_ai_budget().await?;
        let _slot = self.ai_slot().await?;
        let posted = if multimodal {
            post_multimodal_embeddings(&provider, inputs, purpose).await
        } else {
            let texts: Vec<String> = inputs.iter().map(|i| i.text.clone()).collect();
            post_embeddings(&provider, &texts, purpose).await
        };
        match posted {
            Ok((vectors, tokens)) => {
                self.record_ai_usage(feature, tokens, 0, true).await;
                Ok(Embeddings { vectors, model: provider.model, tokens })
            }
            Err(e) => {
                self.record_ai_usage(feature, 0, 0, false).await;
                Err(e)
            }
        }
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
        let _slot = self.ai_slot().await?;
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
        let _slot = self.ai_slot().await?;
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

    /// A slot for one call to a provider (see [`AiGate`]).
    async fn ai_slot(&self) -> Result<AiSlot<'_>> {
        let limit = self.config_value("ai.max_concurrent_calls").as_u64().unwrap_or(8) as usize;
        self.ai_gate.slot(limit, SLOT_WAIT).await
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

/// Lines of a server-sent event stream as they arrive: each `data:` line's
/// payload is handed to `on_data` (which returns false to stop early).
async fn read_sse(mut response: reqwest::Response, mut on_data: impl FnMut(&str) -> Result<bool>) -> Result<()> {
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.context("reading the streamed answer")? {
        buffer.extend_from_slice(&chunk);
        while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(data) = line.strip_prefix("data:") {
                if !on_data(data.trim_start())? {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// [`post_anthropic`], streamed (`"stream": true`): text is sent to `deltas`
/// as it arrives, and the events are put back together into the reply the
/// unstreamed call returns (content blocks, stop reason, usage).
async fn post_anthropic_streamed(provider: &Provider, body: Value, deltas: &tokio::sync::mpsc::UnboundedSender<ChatStream>) -> Result<(Value, u64, u64)> {
    let key = provider.key.as_ref().map(|k| k.expose().to_string()).unwrap_or_default();
    let response = client()?
        .post(format!("{}/v1/messages", provider.base_url))
        .header("x-api-key", &key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .context("calling the Claude API")?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(provider_error("Claude", status, &text, Some(&key)));
    }
    let mut blocks: Vec<Value> = Vec::new();
    // Tool inputs arrive as pieces of JSON text, parsed once the block ends.
    let mut partial: Vec<String> = Vec::new();
    let (mut input, mut output, mut stop_reason, mut stop_details) = (0u64, 0u64, Value::Null, Value::Null);
    read_sse(response, |data| {
        let Ok(event) = serde_json::from_str::<Value>(data) else { return Ok(true) };
        match event["type"].as_str().unwrap_or_default() {
            "message_start" => {
                let usage = &event["message"]["usage"];
                input = usage["input_tokens"].as_u64().unwrap_or(0)
                    + usage["cache_creation_input_tokens"].as_u64().unwrap_or(0)
                    + usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
                output = usage["output_tokens"].as_u64().unwrap_or(0);
            }
            "content_block_start" => {
                let index = event["index"].as_u64().unwrap_or(blocks.len() as u64) as usize;
                while blocks.len() <= index {
                    blocks.push(Value::Null);
                    partial.push(String::new());
                }
                blocks[index] = event["content_block"].clone();
            }
            "content_block_delta" => {
                let index = event["index"].as_u64().unwrap_or(0) as usize;
                let delta = &event["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        let text = delta["text"].as_str().unwrap_or_default();
                        if let Some(block) = blocks.get_mut(index) {
                            let joined = format!("{}{}", block["text"].as_str().unwrap_or_default(), text);
                            block["text"] = json!(joined);
                        }
                        let _ = deltas.send(ChatStream::Text(text.to_string()));
                    }
                    "input_json_delta" => {
                        if let Some(piece) = partial.get_mut(index) {
                            piece.push_str(delta["partial_json"].as_str().unwrap_or_default());
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = event["index"].as_u64().unwrap_or(0) as usize;
                if let (Some(block), Some(piece)) = (blocks.get_mut(index), partial.get(index)) {
                    if block["type"] == "tool_use" {
                        block["input"] = if piece.trim().is_empty() { json!({}) } else { serde_json::from_str(piece).unwrap_or_else(|_| json!({})) };
                    }
                }
            }
            "message_delta" => {
                stop_reason = event["delta"]["stop_reason"].clone();
                stop_details = event["delta"]["stop_details"].clone();
                if let Some(n) = event["usage"]["output_tokens"].as_u64() {
                    output = n;
                }
            }
            "message_stop" => return Ok(false),
            "error" => bail!("The Claude API stopped with an error: {}", event["error"]["message"].as_str().unwrap_or("unknown")),
            _ => {}
        }
        Ok(true)
    })
    .await?;
    let reply = json!({ "content": blocks.into_iter().filter(|b| !b.is_null()).collect::<Vec<_>>(), "stop_reason": stop_reason, "stop_details": stop_details });
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
    let (reply, input, output) = match &request.deltas {
        Some(deltas) => {
            body["stream"] = json!(true);
            post_anthropic_streamed(provider, body, deltas).await?
        }
        None => post_anthropic(provider, body).await?,
    };
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

/// POST texts to an embeddings API (OpenAI's shape, which Voyage, Azure,
/// GitHub Models and OpenAI-compatible servers share); vectors in order, and tokens.
/// Voyage AI's multimodal embeddings: each input's text and picture as one
/// embedding (`POST /multimodalembeddings`).
async fn post_multimodal_embeddings(provider: &Provider, inputs: &[EmbedInput], purpose: EmbedPurpose) -> Result<(Vec<Vec<f32>>, u64)> {
    use base64::Engine as _;
    let items: Vec<Value> = inputs
        .iter()
        .map(|input| {
            let mut content = Vec::new();
            if !input.text.trim().is_empty() {
                content.push(json!({ "type": "text", "text": input.text }));
            }
            if let Some((content_type, bytes)) = &input.image {
                content.push(json!({ "type": "image_base64", "image_base64": format!("data:{};base64,{}", content_type, base64::engine::general_purpose::STANDARD.encode(bytes)) }));
            }
            json!({ "content": content })
        })
        .collect();
    let body = json!({ "model": provider.model, "inputs": items, "input_type": if purpose == EmbedPurpose::Query { "query" } else { "document" } });
    let key = provider.key.as_ref().map(|k| k.expose().to_string()).unwrap_or_default();
    let response = client()?.post(format!("{}/multimodalembeddings", provider.base_url)).bearer_auth(&key).json(&body).send().await.context("calling the Voyage AI multimodal embeddings API")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(provider_error("Voyage AI", status, &text, Some(&key)));
    }
    let reply: Value = serde_json::from_str(&text).context("the Voyage AI multimodal embeddings API's answer wasn't JSON")?;
    let mut items: Vec<(usize, Vec<f32>)> = reply["data"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, item)| crate::vector::extract(&item["embedding"]).map(|v| (item["index"].as_u64().map(|n| n as usize).unwrap_or(i), v)))
        .collect();
    items.sort_by_key(|(i, _)| *i);
    if items.len() != inputs.len() {
        bail!("the Voyage AI multimodal embeddings API returned {} vectors for {} inputs", items.len(), inputs.len());
    }
    let tokens = reply["usage"]["total_tokens"].as_u64().unwrap_or(0);
    Ok((items.into_iter().map(|(_, v)| v).collect(), tokens))
}

async fn post_embeddings(provider: &Provider, texts: &[String], purpose: EmbedPurpose) -> Result<(Vec<Vec<f32>>, u64)> {
    let mut body = json!({ "model": provider.model, "input": texts });
    if provider.kind == "voyage" {
        body["input_type"] = json!(if purpose == EmbedPurpose::Query { "query" } else { "document" });
    }
    let key = provider.key.as_ref().map(|k| k.expose().to_string()).unwrap_or_default();
    let mut call = client()?.post(format!("{}/embeddings", provider.base_url)).json(&body);
    if !key.is_empty() {
        call = if provider.kind == "azure_openai" { call.header("api-key", &key) } else { call.bearer_auth(&key) };
    }
    let name = provider_name(&provider.kind);
    let response = call.send().await.with_context(|| format!("calling the {} embeddings API", name))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(provider_error(name, status, &text, Some(&key)));
    }
    let reply: Value = serde_json::from_str(&text).with_context(|| format!("the {} embeddings API's answer wasn't JSON", name))?;
    let mut items: Vec<(usize, Vec<f32>)> = reply["data"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, item)| {
            let index = item["index"].as_u64().map(|n| n as usize).unwrap_or(i);
            crate::vector::extract(&item["embedding"]).map(|v| (index, v))
        })
        .collect();
    items.sort_by_key(|(i, _)| *i);
    if items.len() != texts.len() {
        bail!("the {} embeddings API returned {} vectors for {} texts", name, items.len(), texts.len());
    }
    let usage = &reply["usage"];
    let tokens = usage["total_tokens"].as_u64().or_else(|| usage["prompt_tokens"].as_u64()).unwrap_or(0);
    Ok((items.into_iter().map(|(_, v)| v).collect(), tokens))
}

fn provider_name(kind: &str) -> &'static str {
    match kind {
        "voyage" => "Voyage AI",
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

/// [`post_chat_completions`], streamed: text is sent to `deltas` as it
/// arrives, and the chunks are put back together into the reply the
/// unstreamed call returns (message with content and tool calls, finish
/// reason, usage).
async fn post_chat_completions_streamed(provider: &Provider, mut body: Map<String, Value>, max_tokens: u64, deltas: &tokio::sync::mpsc::UnboundedSender<ChatStream>) -> Result<(Value, u64, u64)> {
    let limit_name = if matches!(provider.kind.as_str(), "openai" | "azure_openai") { "max_completion_tokens" } else { "max_tokens" };
    body.insert(limit_name.into(), json!(max_tokens));
    body.insert("stream".into(), json!(true));
    // OpenAI's own APIs report usage at the end of a stream only when asked.
    if matches!(provider.kind.as_str(), "openai" | "azure_openai") {
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    let key = provider.key.as_ref().map(|k| k.expose().to_string()).unwrap_or_default();
    let mut call = client()?.post(format!("{}/chat/completions", provider.base_url)).json(&Value::Object(body));
    if !key.is_empty() {
        call = if provider.kind == "azure_openai" { call.header("api-key", &key) } else { call.bearer_auth(&key) };
    }
    let name = provider_name(&provider.kind);
    let response = call.send().await.with_context(|| format!("calling the {} API", name))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(provider_error(name, status, &text, Some(&key)));
    }
    let mut content = String::new();
    // Tool calls arrive in pieces, by index: an id and name first, then argument text.
    let mut calls: Vec<(String, String, String)> = Vec::new();
    let (mut finish, mut input, mut output) = (Value::Null, 0u64, 0u64);
    read_sse(response, |data| {
        if data == "[DONE]" {
            return Ok(false);
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else { return Ok(true) };
        if let Some(message) = chunk["error"]["message"].as_str() {
            bail!("The {} API stopped with an error: {}", name, message);
        }
        if let Some(n) = chunk["usage"]["prompt_tokens"].as_u64() {
            input = n;
        }
        if let Some(n) = chunk["usage"]["completion_tokens"].as_u64() {
            output = n;
        }
        let choice = &chunk["choices"][0];
        if !choice["finish_reason"].is_null() {
            finish = choice["finish_reason"].clone();
        }
        let delta = &choice["delta"];
        if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            content.push_str(text);
            let _ = deltas.send(ChatStream::Text(text.to_string()));
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = call["index"].as_u64().unwrap_or(calls.len() as u64) as usize;
            while calls.len() <= index {
                calls.push(Default::default());
            }
            let slot = &mut calls[index];
            if let Some(id) = call["id"].as_str() {
                slot.0 = id.to_string();
            }
            if let Some(name) = call["function"]["name"].as_str() {
                slot.1.push_str(name);
            }
            if let Some(arguments) = call["function"]["arguments"].as_str() {
                slot.2.push_str(arguments);
            }
        }
        Ok(true)
    })
    .await?;
    let tool_calls: Vec<Value> = calls.into_iter().map(|(id, name, arguments)| json!({ "id": id, "type": "function", "function": { "name": name, "arguments": arguments } })).collect();
    let mut message = json!({ "role": "assistant", "content": content });
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }
    Ok((json!({ "choices": [{ "message": message, "finish_reason": finish }] }), input, output))
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
    let (reply, input, output) = match &request.deltas {
        Some(deltas) => post_chat_completions_streamed(provider, body, max_tokens, deltas).await?,
        None => post_chat_completions(provider, body, max_tokens).await?,
    };
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

    /// A chat completions server that streams one reply: text in two chunks,
    /// a tool call whose arguments come in pieces, and usage at the end.
    async fn streaming_server() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 64 * 1024];
            let _ = socket.read(&mut request).await;
            let chunks = [
                json!({ "choices": [{ "delta": { "role": "assistant", "content": "Let me " } }] }),
                json!({ "choices": [{ "delta": { "content": "count." } }] }),
                json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0, "id": "call_9", "type": "function", "function": { "name": "count", "arguments": "{\"tessel" } }] } }] }),
                json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": "lation\":\"orders\"}" } }] }, "finish_reason": "tool_calls" }] }),
                json!({ "choices": [], "usage": { "prompt_tokens": 21, "completion_tokens": 9 } }),
            ];
            let mut body: String = chunks.iter().map(|c| format!("data: {}\r\n\r\n", c)).collect();
            body.push_str("data: [DONE]\r\n\r\n");
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
        });
        format!("http://{}", address)
    }

    #[tokio::test]
    async fn streamed_chat_completions_put_text_and_tool_calls_back_together() {
        let provider = Provider { kind: "openai_compatible".into(), model: "m".into(), base_url: streaming_server().await, key: None, legacy: false };
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let request = ChatRequest { feature: "chat".into(), system: "s".into(), turns: vec![Turn::User("How many?".into())], tools: Vec::new(), max_tokens: 100, model: None, deltas: Some(sender) };
        let (text, calls, input, output) = chat_completions_with_tools(&provider, &request, 100).await.unwrap();
        assert_eq!(text, "Let me count.");
        assert_eq!(calls.len(), 1);
        assert_eq!((calls[0].id.as_str(), calls[0].name.as_str(), &calls[0].input), ("call_9", "count", &json!({ "tessellation": "orders" })));
        assert_eq!((input, output), (21, 9));
        drop(request);
        let mut pieces = Vec::new();
        while let Some(piece) = receiver.recv().await {
            pieces.push(piece);
        }
        assert_eq!(pieces, vec![ChatStream::Text("Let me ".into()), ChatStream::Text("count.".into())]);
    }

    #[tokio::test]
    async fn ai_calls_wait_for_a_slot_then_give_up() {
        let gate: &'static AiGate = Box::leak(Box::default());
        let first = gate.slot(2, Duration::from_millis(50)).await.unwrap();
        let _second = gate.slot(2, Duration::from_millis(50)).await.unwrap();
        assert_eq!(gate.running(), 2);
        // A third waits, and gets the first's slot when it's given back...
        let waiting = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(first);
        });
        let third = gate.slot(2, Duration::from_secs(5)).await.unwrap();
        waiting.await.unwrap();
        assert_eq!(gate.running(), 2);
        // ...and a fourth gives up when none is freed in time.
        let refused = gate.slot(2, Duration::from_millis(30)).await.err().unwrap();
        assert!(refused.to_string().contains("ai.max_concurrent_calls"), "{}", refused);
        drop(third);
        assert_eq!(gate.running(), 1);
        // 0: no limit.
        let many: Vec<_> = futures::future::join_all((0..5).map(|_| gate.slot(0, Duration::ZERO))).await;
        assert!(many.iter().all(Result::is_ok));
    }

    #[tokio::test]
    async fn pictures_are_embedded_with_their_text_by_multimodal_models() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = vec![0u8; 64 * 1024];
            // Headers, then the body by its length.
            loop {
                let n = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..n]);
                let text = String::from_utf8_lossy(&request).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let length: usize = text.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())).unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let _ = seen_tx.send(String::from_utf8_lossy(&request).to_string());
            let body = json!({ "data": [{ "embedding": [0.1, 0.2], "index": 1 }, { "embedding": [0.3, 0.4], "index": 0 }], "usage": { "total_tokens": 1200 } }).to_string();
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
        });
        let provider = Provider { kind: "voyage".into(), model: "voyage-multimodal-3".into(), base_url: format!("http://{}", address), key: None, legacy: false };
        assert!(takes_images(&provider.kind, &provider.model) && !takes_images("voyage", "voyage-3.5") && !takes_images("openai", "voyage-multimodal-3"));
        let inputs = vec![
            EmbedInput { text: "sunset.jpg".into(), image: Some(("image/jpeg".into(), vec![0xFF, 0xD8, 0xFF])) },
            EmbedInput { text: String::new(), image: Some(("image/webp".into(), vec![1, 2, 3])) },
        ];
        let (vectors, tokens) = post_multimodal_embeddings(&provider, &inputs, EmbedPurpose::Document).await.unwrap();
        assert_eq!(vectors, vec![vec![0.3, 0.4], vec![0.1, 0.2]], "in the inputs' order");
        assert_eq!(tokens, 1200);
        let request = seen_rx.await.unwrap();
        assert!(request.starts_with("POST /multimodalembeddings "), "{}", &request[..60]);
        let body: Value = serde_json::from_str(&request[request.find("\r\n\r\n").unwrap() + 4..]).unwrap();
        assert_eq!(body["model"], "voyage-multimodal-3");
        assert_eq!(body["input_type"], "document");
        assert_eq!(body["inputs"][0]["content"], json!([{ "type": "text", "text": "sunset.jpg" }, { "type": "image_base64", "image_base64": "data:image/jpeg;base64,/9j/" }]));
        assert_eq!(body["inputs"][1]["content"], json!([{ "type": "image_base64", "image_base64": "data:image/webp;base64,AQID" }]), "no empty text part");
    }

    #[test]
    fn provider_errors_never_carry_the_key() {
        let e = provider_error("Claude", reqwest::StatusCode::UNAUTHORIZED, "invalid x-api-key sk-ant-secret-123456", Some("sk-ant-secret-123456"));
        assert!(!e.to_string().contains("sk-ant-secret"));
    }
}
