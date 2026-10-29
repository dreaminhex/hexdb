// HexDB Core Lattice Configuration
//
// Settings that belong to the whole lattice rather than one machine: which
// AI provider to use and what AI features may do. Admins change them through `PUT /config` (the admin UI and `hexdb
// config` use it); `hexdb.toml` keeps what belongs to one hex (addresses,
// data directory, keys).
//
// Every change writes a new, immutable version as a document in the `_config`
// system tessellation, so versions replicate to every hex with the rest of the
// data, survive an Overseer failover (the new Overseer already holds them) and
// reach new hexes in their first sync. A version holds the whole configuration,
// a SHA-256 over its canonical JSON, and who made it and why. Rolling back
// writes a new version with an earlier version's contents; history is never
// rewritten.
//
// Secrets (API keys) are sealed with the lattice transfer
// key, bound to their setting name, and are never returned by any route, logged
// or written to the audit trail: reads show only whether a secret is set, when,
// by whom, and a keyed fingerprint to tell two values apart. An admin can
// replace or clear a secret, never read it back.
//
// Each hex applies the newest version after checking its checksum, its
// settings against the schema below, and that every secret opens. A version
// that fails is rejected on that hex (the previous one stays in effect) and
// reported in `/status` and the lattice view, never half-applied.
//
// Every setting is lattice-wide. Versions written by 1.6 previews may hold
// remote file storage settings (`file_storage.*`) and per-hex overrides of
// them; both were removed, so they're ignored when applied and dropped from
// the next version written.

use crate::engine::{DocumentQuery, EngineError, HexDBEngine};
use crate::filter::{Filter, SortKey};
use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};
use ulid::Ulid;

/// System tessellation holding the configuration versions.
pub const CONFIG_TESSELLATION: &str = "_config";
/// Format of a version's body (bumped if its meaning changes).
const FORMAT: u32 = 1;
/// Settings from 1.6 previews that no longer exist (remote file storage).
const RETIRED_PREFIX: &str = "file_storage.";
const MAX_TEXT: usize = 2048;
const MAX_SECRET: usize = 16 * 1024;
const MAX_COMMENT: usize = 500;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ConfigKind {
    Text,
    /// An http(s) URL, or empty.
    Url,
    /// Write-only.
    Secret,
    Boolean,
    Integer { min: i64, max: i64 },
    /// One of these.
    Choice { choices: &'static [&'static str] },
    /// Any of these (a list without repeats).
    Set { choices: &'static [&'static str] },
    /// Web origins (`https://app.example.com`), comma-separated, or `*`.
    Origins,
}

/// One configurable setting.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ConfigDef {
    pub key: &'static str,
    pub kind: ConfigKind,
    /// The default, as JSON text (secrets have none).
    #[serde(skip)]
    default: &'static str,
    pub description: &'static str,
}

impl ConfigDef {
    pub fn default_value(&self) -> Value {
        serde_json::from_str(self.default).unwrap_or(Value::Null)
    }

    pub fn is_secret(&self) -> bool {
        self.kind == ConfigKind::Secret
    }
}

const fn def(key: &'static str, kind: ConfigKind, default: &'static str, description: &'static str) -> ConfigDef {
    ConfigDef { key, kind, default, description }
}

const fn text(key: &'static str, description: &'static str) -> ConfigDef {
    def(key, ConfigKind::Text, "\"\"", description)
}

const fn secret(key: &'static str, description: &'static str) -> ConfigDef {
    def(key, ConfigKind::Secret, "null", description)
}

pub const AI_PROVIDERS: &[&str] = &["none", "anthropic", "openai", "github", "azure_openai", "openai_compatible"];
pub const EMBEDDING_PROVIDERS: &[&str] = &["same", "none", "openai", "voyage", "azure_openai", "github", "openai_compatible"];
pub const AI_INPUTS: &[&str] = &["metadata", "text", "thumbnail", "image", "video", "pdf"];

pub const CONFIG: &[ConfigDef] = &[
    def("ai.provider", ConfigKind::Choice { choices: AI_PROVIDERS }, "\"none\"", "AI provider for AI features: Anthropic (Claude), OpenAI, GitHub Models, Azure OpenAI, or any OpenAI-compatible endpoint (e.g. Ollama). Calls use this lattice's own key."),
    text("ai.model", "Model name, e.g. the provider's model ID (empty: the provider's default)."),
    def("ai.base_url", ConfigKind::Url, "\"\"", "API base URL (empty: the provider's public API). Required for Azure OpenAI and OpenAI-compatible endpoints."),
    secret("ai.api_key", "API key for the AI provider."),
    def("ai.monthly_token_budget", ConfigKind::Integer { min: 0, max: 1_000_000_000_000 }, "1000000", "Most tokens AI features may use per calendar month; they pause when it is reached (0: no limit)."),
    def("ai.max_tokens_per_file", ConfigKind::Integer { min: 100, max: 1_000_000 }, "4000", "Most tokens one file may use (input and output)."),
    def("ai.tagging.enabled", ConfigKind::Boolean, "false", "Let the AI add tags and a description to files as they are uploaded."),
    text("ai.tagging.tessellations", "Files tessellations whose new files are tagged, comma-separated (empty: every files tessellation)."),
    def("ai.tagging.send", ConfigKind::Set { choices: AI_INPUTS }, "[\"metadata\",\"text\"]", "What auto-tagging sends to the AI: extracted metadata, extracted text, the thumbnail, the full image, frames from a video, the whole PDF (Anthropic and OpenAI only)."),
    def("ai.tagging.video_frames", ConfigKind::Integer { min: 1, max: 16 }, "4", "With video in what tagging sends: frames sent from each H.264 video, spread through it."),
    def("ai.video_thumbnails.enabled", ConfigKind::Boolean, "false", "Let the AI pick the most representative frame for video thumbnails."),
    def("ai.video_thumbnails.frames", ConfigKind::Integer { min: 2, max: 16 }, "6", "Candidate frames the AI chooses from."),
    def("quotas.default_mb", ConfigKind::Integer { min: 0, max: 1_000_000_000 }, "0", "Upload quota per user in MB, for users without their own (0: none). Administrators get only a quota of their own."),
    def("quotas.default_files", ConfigKind::Integer { min: 0, max: 1_000_000_000 }, "0", "Upload quota per user in files, for users without their own (0: none)."),
    def("sharing.enabled", ConfigKind::Boolean, "true", "Let users with the share permission make sharing links to files and folders for people without an account."),
    def("sharing.default_days", ConfigKind::Integer { min: 0, max: 3650 }, "7", "Days a new link lasts when its maker doesn't say (0: no expiry, if sharing.max_days allows it)."),
    def("sharing.max_days", ConfigKind::Integer { min: 0, max: 3650 }, "30", "Longest a link may last, in days (0: links may never expire)."),
    def("sharing.require_password", ConfigKind::Boolean, "false", "Every new link needs a password."),
    def("cors.allowed_origins", ConfigKind::Origins, "\"\"", "Web pages on other origins that may call the API from a browser, comma-separated (e.g. https://app.example.com), or * for any. They sign in with an API key or a bearer token; the session cookie never works across origins. Empty: none."),
    def("a2a.enabled", ConfigKind::Boolean, "false", "Let other AI agents ask this lattice questions over A2A (Agent2Agent): an agent card at /.well-known/agent-card.json and JSON-RPC at /a2a. Answers come from the AI chat, as the calling user, within the chat's limits; ai.chat.enabled must be on too."),
    def("ai.embeddings.provider", ConfigKind::Choice { choices: EMBEDDING_PROVIDERS }, "\"same\"", "Embeddings for vector indexes with source fields and for similarity searches by text: same (ai.provider, with its key; Anthropic has no embeddings API), none, OpenAI, Voyage AI (Anthropic's recommended embeddings), Azure OpenAI, GitHub Models, or an OpenAI-compatible server (e.g. Ollama)."),
    text("ai.embeddings.model", "Embedding model (empty: text-embedding-3-small for OpenAI and GitHub Models, voyage-3.5 for Voyage). Changing it re-embeds every document of an auto-embedding vector index."),
    def("ai.embeddings.base_url", ConfigKind::Url, "\"\"", "Embeddings API base URL (empty: the provider's public API, or ai.base_url with the same provider). Required for Azure OpenAI and OpenAI-compatible servers."),
    secret("ai.embeddings.api_key", "API key for the embeddings provider (empty with the same provider: ai.api_key)."),
    def("ai.embeddings.user_daily_tokens", ConfigKind::Integer { min: 0, max: 1_000_000_000 }, "200000", "Most tokens one user's similarity searches by text may use per day (0: no limit beyond the monthly budget)."),
    def("ai.chat.enabled", ConfigKind::Boolean, "false", "Let users with the chat permission ask the AI about the data they can read (the admin UI's Ask page and POST /ai/chat)."),
    text("ai.chat.model", "Model for the chat (empty: ai.model). A smaller, cheaper model is usually enough, e.g. claude-haiku-5-5."),
    def("ai.chat.max_tokens_per_answer", ConfigKind::Integer { min: 64, max: 16_000 }, "1024", "Most tokens one chat answer may use."),
    def("ai.chat.max_tokens_per_question", ConfigKind::Integer { min: 1000, max: 1_000_000 }, "20000", "Most tokens one question may use in all, input and output across every lookup (a question resends the conversation and the lookups' results each step)."),
    def("ai.chat.max_tool_calls", ConfigKind::Integer { min: 0, max: 20 }, "6", "Most lookups (queries, searches) the AI may make to answer one question."),
    def("ai.chat.max_question_chars", ConfigKind::Integer { min: 50, max: 20_000 }, "2000", "Longest question, in characters."),
    def("ai.chat.max_history", ConfigKind::Integer { min: 0, max: 50 }, "10", "Earlier questions and answers sent along with a question (0: none)."),
    def("ai.chat.user_daily_tokens", ConfigKind::Integer { min: 0, max: 1_000_000_000 }, "100000", "Most chat tokens one user may use per day (0: no limit beyond the monthly budget)."),
    def("ai.chat.client_daily_tokens", ConfigKind::Integer { min: 0, max: 1_000_000_000 }, "50000", "Most chat tokens one client address may use per day (0: no limit). Separates visitors who share a login, as on a public demo."),
    def("ai.chat.client_questions_per_minute", ConfigKind::Integer { min: 0, max: 1000 }, "6", "Most questions one client address may ask per minute (0: no limit)."),
];

pub fn definition(key: &str) -> Option<&'static ConfigDef> {
    CONFIG.iter().find(|d| d.key == key)
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    EngineError::Invalid(message.into()).into()
}

/// Check a (non-secret) value against its definition; returns it normalized.
/// Messages never include the value.
pub fn validate(def: &ConfigDef, value: &Value) -> Result<Value> {
    let key = def.key;
    match def.kind {
        ConfigKind::Secret => match value.as_str() {
            Some(s) if !s.trim().is_empty() && s.len() <= MAX_SECRET => Ok(Value::String(s.trim().to_string())),
            Some(s) if s.len() > MAX_SECRET => Err(invalid(format!("{} is longer than {} bytes.", key, MAX_SECRET))),
            _ => Err(invalid(format!("{} must be a non-empty string (use null to clear it).", key))),
        },
        ConfigKind::Text => {
            let s = value.as_str().ok_or_else(|| invalid(format!("{} must be a string.", key)))?.trim();
            if s.len() > MAX_TEXT || s.chars().any(char::is_control) {
                return Err(invalid(format!("{} must be at most {} characters with no control characters.", key, MAX_TEXT)));
            }
            Ok(Value::String(s.to_string()))
        }
        ConfigKind::Url => {
            let s = value.as_str().ok_or_else(|| invalid(format!("{} must be a string.", key)))?.trim();
            if !s.is_empty() {
                let ok = (s.starts_with("https://") || s.starts_with("http://"))
                    && s.len() <= MAX_TEXT
                    && !s.chars().any(|c| c.is_control() || c.is_whitespace());
                if !ok {
                    return Err(invalid(format!("{} must be an http:// or https:// URL, or empty.", key)));
                }
                if is_metadata_host(s) {
                    return Err(invalid(format!("{} can't point at a cloud metadata service.", key)));
                }
            }
            Ok(Value::String(s.trim_end_matches('/').to_string()))
        }
        ConfigKind::Origins => {
            let s = value.as_str().ok_or_else(|| invalid(format!("{} must be a string.", key)))?;
            let mut origins: Vec<String> = Vec::new();
            for item in s.split(|c: char| c == ',' || c.is_whitespace()).filter(|i| !i.is_empty()) {
                let origin = normalize_origin(item).ok_or_else(|| {
                    invalid(format!("{} must list origins as scheme://host[:port] with no path (e.g. https://app.example.com), or be *.", key))
                })?;
                if !origins.contains(&origin) {
                    origins.push(origin);
                }
            }
            if origins.len() > 1 && origins.iter().any(|o| o == "*") {
                return Err(invalid(format!("{}: * already allows every origin; list nothing else with it.", key)));
            }
            let joined = origins.join(", ");
            if joined.len() > MAX_TEXT {
                return Err(invalid(format!("{} must be at most {} characters.", key, MAX_TEXT)));
            }
            Ok(Value::String(joined))
        }
        ConfigKind::Boolean => value.as_bool().map(Value::Bool).ok_or_else(|| invalid(format!("{} must be true or false.", key))),
        ConfigKind::Integer { min, max } => {
            let n = value.as_i64().ok_or_else(|| invalid(format!("{} must be a whole number.", key)))?;
            if n < min || n > max {
                return Err(invalid(format!("{} must be between {} and {}.", key, min, max)));
            }
            Ok(Value::from(n))
        }
        ConfigKind::Choice { choices } => match value.as_str() {
            Some(s) if choices.contains(&s) => Ok(Value::String(s.to_string())),
            _ => Err(invalid(format!("{} must be one of: {}.", key, choices.join(", ")))),
        },
        ConfigKind::Set { choices } => {
            let items = value.as_array().ok_or_else(|| invalid(format!("{} must be a list.", key)))?;
            let mut seen = BTreeSet::new();
            for item in items {
                match item.as_str() {
                    Some(s) if choices.contains(&s) => {
                        seen.insert(s);
                    }
                    _ => return Err(invalid(format!("{} may only contain: {}.", key, choices.join(", ")))),
                }
            }
            // Keep the schema's order, so equal sets compare equal.
            Ok(Value::Array(choices.iter().filter(|c| seen.contains(*c)).map(|c| Value::String(c.to_string())).collect()))
        }
    }
}

/// A web origin as browsers send it (`scheme://host[:port]`, lowercase, no
/// default port), or `*`. None if `item` isn't one.
pub fn normalize_origin(item: &str) -> Option<String> {
    if item == "*" {
        return Some(item.to_string());
    }
    let item = item.trim_end_matches('/').to_ascii_lowercase();
    let (scheme, authority) = item.split_once("://")?;
    if !matches!(scheme, "http" | "https") || authority.is_empty() {
        return None;
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(rest) => {
            let (inner, after) = rest.split_once(']')?;
            inner.parse::<std::net::Ipv6Addr>().ok()?;
            (format!("[{}]", inner), if after.is_empty() { None } else { Some(after.strip_prefix(':')?.parse::<u16>().ok()?) })
        }
        None => {
            let (host, port) = match authority.split_once(':') {
                Some((host, port)) => (host, Some(port.parse::<u16>().ok()?)),
                None => (authority, None),
            };
            let label_ok = |l: &str| !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
            if host.len() > 253 || !host.split('.').all(label_ok) {
                return None;
            }
            (host.to_string(), port)
        }
    };
    let default_port = if scheme == "https" { 443 } else { 80 };
    Some(match port {
        Some(p) if p != default_port => format!("{}://{}:{}", scheme, host, p),
        _ => format!("{}://{}", scheme, host),
    })
}

/// True for the well-known cloud instance metadata endpoints (they hand out
/// credentials, and a provider's error text could echo them back). A name that
/// merely resolves to one isn't caught; this is defense in depth for settings
/// only administrators can change.
fn is_metadata_host(url: &str) -> bool {
    let after = url.split("://").nth(1).unwrap_or_default();
    let authority = after.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = if host.starts_with('[') { host.split(']').next().unwrap_or_default().trim_start_matches('[') } else { host.split(':').next().unwrap_or_default() };
    let host = host.to_ascii_lowercase();
    matches!(host.as_str(), "169.254.169.254" | "169.254.170.2" | "fd00:ec2::254" | "metadata.google.internal" | "metadata.goog" | "metadata" | "100.100.100.200")
}


// ---------------------------------------------------------------------------
// Versions
// ---------------------------------------------------------------------------

/// A secret as stored: sealed with the lattice transfer key, bound to its setting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SealedSecret {
    /// Base64 of the sealed value.
    pub sealed: String,
    /// Keyed hash prefix of the value (tells two values apart; reveals nothing).
    pub fingerprint: String,
    pub updated: i64,
    pub updated_by: String,
}

/// Settings for one scope (the lattice; in 1.6 previews also one hex's overrides).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigScope {
    #[serde(default)]
    pub settings: BTreeMap<String, Value>,
    #[serde(default)]
    pub secrets: BTreeMap<String, SealedSecret>,
}

/// One configuration version, as hashed and stored.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigVersion {
    pub format: u32,
    pub version: u64,
    /// The version this one was made from (0 for the first).
    pub parent: u64,
    pub time: i64,
    pub author: String,
    #[serde(default)]
    pub comment: String,
    /// The version whose contents this one restores.
    #[serde(default)]
    pub rollback_of: Option<u64>,
    pub lattice: ConfigScope,
    /// Per-hex overrides from 1.6 previews, by hex name: ignored, and empty in
    /// versions written since (kept so older versions keep their checksums).
    #[serde(default)]
    pub hexes: BTreeMap<String, ConfigScope>,
}

impl ConfigVersion {
    /// Canonical JSON (struct fields in order, maps sorted) and its SHA-256.
    pub fn canonical(&self) -> (String, String) {
        let body = serde_json::to_string(self).expect("a config version serializes");
        let sha = hex::encode(Sha256::digest(body.as_bytes()));
        (body, sha)
    }
}

fn doc_id(version: u64) -> Ulid {
    Ulid::from(version as u128)
}

/// Seal a secret for storage in a version.
fn seal(engine: &HexDBEngine, key: &str, value: &str, actor: &str) -> SealedSecret {
    SealedSecret {
        sealed: STANDARD.encode(engine.lattice_keys.seal(value.as_bytes(), secret_aad(key).as_bytes())),
        fingerprint: engine.lattice_keys.fingerprint(value.as_bytes()),
        updated: Utc::now().timestamp_millis(),
        updated_by: actor.to_string(),
    }
}

fn secret_aad(key: &str) -> String {
    format!("hexdb-config-secret:{}", key)
}

fn open(engine: &HexDBEngine, key: &str, secret: &SealedSecret) -> Result<Secret> {
    let bytes = STANDARD.decode(&secret.sealed).map_err(|_| anyhow!("{} is not valid base64", key))?;
    let plain = engine
        .lattice_keys
        .open(&bytes, secret_aad(key).as_bytes())
        .ok_or_else(|| anyhow!("{} can't be decrypted with this hex's lattice keys", key))?;
    Ok(Secret(String::from_utf8(plain).map_err(|_| anyhow!("{} is not text", key))?))
}

/// A secret's value in memory. Never printed or serialized.
#[derive(Clone, PartialEq)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// A secret read from the environment (the pre-1.6 advisor key).
    pub(crate) fn from_env(value: &str) -> Self {
        Secret(value.to_string())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// A stored version and whether it checks out.
#[derive(Debug, Clone)]
pub struct StoredVersion {
    pub version: u64,
    pub sha256: String,
    /// The parsed body when it matches its checksum.
    pub body: Option<ConfigVersion>,
    /// Why it doesn't, when it doesn't.
    pub problem: Option<String>,
}

fn parse_stored(data: &Value) -> StoredVersion {
    let version = data.get("version").and_then(Value::as_u64).unwrap_or(0);
    let sha256 = data.get("sha256").and_then(Value::as_str).unwrap_or_default().to_string();
    let Some(body) = data.get("body").and_then(Value::as_str) else {
        return StoredVersion { version, sha256, body: None, problem: Some("the version has no body".into()) };
    };
    let actual = hex::encode(Sha256::digest(body.as_bytes()));
    if actual != sha256 {
        return StoredVersion { version, sha256, body: None, problem: Some("checksum mismatch: the stored version is damaged or incomplete".into()) };
    }
    match serde_json::from_str::<ConfigVersion>(body) {
        Ok(parsed) if parsed.version == version => StoredVersion { version, sha256, body: Some(parsed), problem: None },
        Ok(_) => StoredVersion { version, sha256, body: None, problem: Some("the version number doesn't match its body".into()) },
        Err(e) => StoredVersion { version, sha256, body: None, problem: Some(format!("unreadable body: {}", e)) },
    }
}

// ---------------------------------------------------------------------------
// What a hex applies
// ---------------------------------------------------------------------------

/// The configuration in effect on this hex.
#[derive(Debug, Clone, Default)]
pub struct AppliedConfig {
    /// The version in effect (0: none stored yet, defaults apply).
    pub version: u64,
    pub sha256: String,
    /// Every setting's effective value on this hex (defaults filled in; secrets excluded).
    pub values: BTreeMap<String, Value>,
    pub secrets: BTreeMap<String, Secret>,
    /// A newer version this hex refused, and why.
    pub rejected: Option<(u64, String)>,
}

impl AppliedConfig {
    fn defaults() -> Self {
        AppliedConfig { values: CONFIG.iter().filter(|d| !d.is_secret()).map(|d| (d.key.to_string(), d.default_value())).collect(), ..Default::default() }
    }

    /// `applied`, `rejected` (a newer version failed here) or `default` (no version yet).
    pub fn state(&self) -> &'static str {
        if self.rejected.is_some() {
            "rejected"
        } else if self.version == 0 {
            "default"
        } else {
            "applied"
        }
    }

    pub fn status_json(&self) -> Value {
        json!({
            "version": self.version,
            "sha256": self.sha256,
            "state": self.state(),
            "rejected_version": self.rejected.as_ref().map(|r| r.0),
            "error": self.rejected.as_ref().map(|r| r.1.clone()),
        })
    }
}

/// Engine state for the lattice configuration.
pub struct LatticeConfigState {
    applied: std::sync::RwLock<AppliedConfig>,
    /// `_config` generation last looked at.
    seen: std::sync::Mutex<Option<u64>>,
    /// Serializes changes (on the Overseer).
    write_lock: tokio::sync::Mutex<()>,
}

impl Default for LatticeConfigState {
    fn default() -> Self {
        LatticeConfigState { applied: std::sync::RwLock::new(AppliedConfig::defaults()), seen: Default::default(), write_lock: Default::default() }
    }
}

/// Check a version and work out what it means for this hex.
fn resolve(engine: &HexDBEngine, body: &ConfigVersion) -> Result<AppliedConfig> {
    if body.format > FORMAT {
        bail!("written by a newer HexDB (format {})", body.format);
    }
    let retired = |key: &str| key.starts_with(RETIRED_PREFIX);
    for (key, value) in body.lattice.settings.iter().filter(|(k, _)| !retired(k)) {
        let def = definition(key).ok_or_else(|| anyhow!("unknown setting {} (written by a newer HexDB?)", key))?;
        if def.is_secret() {
            bail!("{} isn't allowed here", key);
        }
        validate(def, value).map_err(|e| anyhow!("{}", e))?;
    }
    let mut applied = AppliedConfig::defaults();
    for key in body.lattice.secrets.keys().filter(|k| !retired(k)) {
        let def = definition(key).ok_or_else(|| anyhow!("unknown secret {} (written by a newer HexDB?)", key))?;
        if !def.is_secret() {
            bail!("{} isn't allowed here", key);
        }
        applied.secrets.insert(key.clone(), open(engine, key, &body.lattice.secrets[key])?);
    }
    for (key, value) in body.lattice.settings.iter().filter(|(k, _)| !retired(k)) {
        applied.values.insert(key.clone(), value.clone());
    }
    applied.version = body.version;
    applied.sha256 = body.canonical().1;
    Ok(applied)
}

/// A version's contents without what 1.6 previews could hold and this build
/// doesn't: remote storage settings and per-hex overrides.
fn without_retired(mut scope: ConfigScope) -> ConfigScope {
    scope.settings.retain(|k, _| !k.starts_with(RETIRED_PREFIX));
    scope.secrets.retain(|k, _| !k.starts_with(RETIRED_PREFIX));
    scope
}

// ---------------------------------------------------------------------------
// Changes
// ---------------------------------------------------------------------------

/// A change request: `changes` maps setting keys to new values (`null` clears).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigChange {
    pub changes: Map<String, Value>,
    #[serde(default)]
    pub comment: Option<String>,
    /// Refuse with a conflict unless this is still the newest version.
    #[serde(default)]
    pub base_version: Option<u64>,
}

/// What a change did, for the audit trail (no values of secrets).
#[derive(Debug, Clone, Default, Serialize)]
pub struct ChangeSummary {
    pub version: u64,
    pub changed: Vec<String>,
    pub cleared: Vec<String>,
    pub secrets_set: Vec<String>,
    pub secrets_cleared: Vec<String>,
}

fn comment_of(comment: Option<String>) -> Result<String> {
    let comment = comment.unwrap_or_default().trim().to_string();
    if comment.chars().count() > MAX_COMMENT {
        return Err(invalid(format!("Comments are at most {} characters.", MAX_COMMENT)));
    }
    Ok(comment)
}

/// Differences between two versions' scopes: (changed or added, removed) keys,
/// with secrets compared by fingerprint and time.
fn scope_diff(old: &ConfigScope, new: &ConfigScope) -> (Vec<String>, Vec<String>) {
    let mut changed = Vec::new();
    let mut removed = Vec::new();
    for (key, value) in &new.settings {
        if old.settings.get(key) != Some(value) {
            changed.push(key.clone());
        }
    }
    for (key, value) in &new.secrets {
        if old.secrets.get(key) != Some(value) {
            changed.push(key.clone());
        }
    }
    for key in old.settings.keys().filter(|k| !new.settings.contains_key(*k)) {
        removed.push(key.clone());
    }
    for key in old.secrets.keys().filter(|k| !new.secrets.contains_key(*k)) {
        removed.push(key.clone());
    }
    changed.sort();
    removed.sort();
    (changed, removed)
}

/// Every difference between two versions, keyed `key` or `hex:<name>/key`.
pub fn version_diff(old: &ConfigVersion, new: &ConfigVersion) -> Value {
    let (mut changed, mut removed) = scope_diff(&old.lattice, &new.lattice);
    let empty = ConfigScope::default();
    let names: BTreeSet<&String> = old.hexes.keys().chain(new.hexes.keys()).collect();
    for name in names {
        let (c, r) = scope_diff(old.hexes.get(name).unwrap_or(&empty), new.hexes.get(name).unwrap_or(&empty));
        changed.extend(c.into_iter().map(|k| format!("hex:{}/{}", name, k)));
        removed.extend(r.into_iter().map(|k| format!("hex:{}/{}", name, k)));
    }
    json!({ "changed": changed, "removed": removed })
}

fn secret_info(secret: &SealedSecret) -> Value {
    json!({ "set": true, "fingerprint": secret.fingerprint, "updated": secret.updated, "updated_by": secret.updated_by })
}

/// A scope for display: settings with their values, secrets as `secret_info`.
fn scope_view(scope: &ConfigScope) -> Value {
    let mut out = Map::new();
    for (key, value) in &scope.settings {
        out.insert(key.clone(), value.clone());
    }
    for (key, secret) in &scope.secrets {
        out.insert(key.clone(), secret_info(secret));
    }
    Value::Object(out)
}

/// A version for display, without secret values.
pub fn version_view(body: &ConfigVersion) -> Value {
    json!({
        "version": body.version,
        "parent": body.parent,
        "time": body.time,
        "author": body.author,
        "comment": body.comment,
        "rollback_of": body.rollback_of,
        "sha256": body.canonical().1,
        "lattice": scope_view(&body.lattice),
        "hexes": body.hexes.iter().map(|(name, scope)| (name.clone(), scope_view(scope))).collect::<Map<String, Value>>(),
    })
}

impl HexDBEngine {
    /// The configuration in effect on this hex.
    pub fn lattice_config(&self) -> AppliedConfig {
        self.lattice_config.applied.read().unwrap().clone()
    }

    /// A setting's effective value on this hex (its default when unset).
    pub fn config_value(&self, key: &str) -> Value {
        self.lattice_config.applied.read().unwrap().values.get(key).cloned().unwrap_or(Value::Null)
    }

    /// A secret's value on this hex, when set.
    pub fn config_secret(&self, key: &str) -> Option<Secret> {
        self.lattice_config.applied.read().unwrap().secrets.get(key).cloned()
    }

    /// Stored versions, newest first.
    async fn stored_versions(&self, limit: usize) -> Result<Vec<StoredVersion>> {
        if !self.tessellation_exists(CONFIG_TESSELLATION) {
            return Ok(Vec::new());
        }
        let page = self
            .query_documents(
                CONFIG_TESSELLATION,
                &DocumentQuery {
                    filter: Filter::parse(&json!({}))?,
                    sort: vec![SortKey { field: "version".into(), descending: true }],
                    offset: 0,
                    limit,
                    after: None,
                    with_total: false,
                },
            )
            .await?;
        Ok(page.documents.iter().map(|d| parse_stored(&d.data_json())).collect())
    }

    async fn stored_version(&self, version: u64) -> Result<Option<StoredVersion>> {
        Ok(self.get_system_document(CONFIG_TESSELLATION, doc_id(version)).await?.map(|d| parse_stored(&d.data_json())))
    }

    /// The newest version, which must check out (for building the next one).
    async fn head(&self) -> Result<ConfigVersion> {
        match self.stored_versions(1).await?.into_iter().next() {
            None => Ok(ConfigVersion { format: FORMAT, ..Default::default() }),
            Some(StoredVersion { body: Some(body), .. }) => Ok(body),
            Some(StoredVersion { version, problem, .. }) => Err(EngineError::Conflict(format!(
                "Configuration version {} failed verification ({}); roll back to an earlier version first.",
                version,
                problem.unwrap_or_default()
            ))
            .into()),
        }
    }

    /// Apply the newest stored version if it changed since the last look.
    /// Cheap when nothing changed; called by a background task on every hex.
    pub async fn refresh_lattice_config(&self) {
        let generation = self.tessellation_generation(CONFIG_TESSELLATION);
        if *self.lattice_config.seen.lock().unwrap() == Some(generation) {
            return;
        }
        let newest = match self.stored_versions(1).await {
            Ok(v) => v.into_iter().next(),
            Err(e) => {
                warn!("⚠️ Couldn't read the lattice configuration: {:#}", e);
                return;
            }
        };
        *self.lattice_config.seen.lock().unwrap() = Some(generation);
        let Some(stored) = newest else { return };
        let current = self.lattice_config.applied.read().unwrap().clone();
        if current.version == stored.version && current.sha256 == stored.sha256 && current.rejected.is_none() {
            return;
        }
        let outcome = match &stored.body {
            Some(body) => resolve(self, body),
            None => Err(anyhow!("{}", stored.problem.clone().unwrap_or_default())),
        };
        {
            let mut applied = self.lattice_config.applied.write().unwrap();
            match outcome {
                Ok(next) => {
                    info!("⚙️ Applied lattice configuration version {} ({}).", next.version, &next.sha256[..12]);
                    *applied = next;
                }
                Err(e) => {
                    if applied.rejected.as_ref().map(|r| r.0) != Some(stored.version) {
                        warn!("⚠️ Rejected lattice configuration version {}: {:#}. Version {} stays in effect.", stored.version, e, applied.version);
                    }
                    applied.rejected = Some((stored.version, format!("{:#}", e)));
                }
            }
        }
        self.check_embedding_model().await;
    }

    /// Store a new version on the Overseer and apply it.
    async fn commit_config(&self, body: ConfigVersion) -> Result<()> {
        let (text, sha) = body.canonical();
        let doc = json!({ "version": body.version, "time": body.time, "author": body.author, "sha256": sha, "body": text });
        // Check it as every hex will before anyone sees it.
        resolve(self, &body).map_err(|e| invalid(format!("{:#}", e)))?;
        self.put_system_document(CONFIG_TESSELLATION, doc_id(body.version), doc, None).await?;
        self.refresh_lattice_config().await;
        Ok(())
    }

    /// Change settings (on the Overseer). Returns what changed; no new version
    /// is written when nothing did.
    pub async fn update_lattice_config(&self, actor: &str, request: ConfigChange) -> Result<(ConfigVersion, ChangeSummary)> {
        self.ensure_writable()?;
        let _guard = self.lattice_config.write_lock.lock().await;
        let head = self.head().await?;
        if let Some(base) = request.base_version {
            if base != head.version {
                return Err(EngineError::Conflict(format!(
                    "The configuration changed since version {} (now {}); reload and try again.",
                    base, head.version
                ))
                .into());
            }
        }
        if request.changes.is_empty() {
            return Err(invalid("No changes given."));
        }
        let comment = comment_of(request.comment)?;
        let mut next = head.clone();
        next.lattice = without_retired(next.lattice);
        next.hexes.clear();
        let scope = &mut next.lattice;
        let mut summary = ChangeSummary::default();
        for (key, value) in &request.changes {
            let def = definition(key).ok_or_else(|| invalid(format!("Unknown setting '{}'.", key)))?;
            if value.is_null() {
                let removed = if def.is_secret() { scope.secrets.remove(key).is_some() } else { scope.settings.remove(key).is_some() };
                if removed {
                    if def.is_secret() { &mut summary.secrets_cleared } else { &mut summary.cleared }.push(key.clone());
                }
                continue;
            }
            let value = validate(def, value)?;
            if def.is_secret() {
                let plain = value.as_str().unwrap_or_default();
                scope.secrets.insert(key.clone(), seal(self, key, plain, actor));
                summary.secrets_set.push(key.clone());
            } else if scope.settings.get(key) != Some(&value) {
                scope.settings.insert(key.clone(), value);
                summary.changed.push(key.clone());
            }
        }
        if summary.changed.is_empty() && summary.cleared.is_empty() && summary.secrets_set.is_empty() && summary.secrets_cleared.is_empty() {
            summary.version = head.version;
            return Ok((head, summary));
        }
        next.format = FORMAT;
        next.version = head.version + 1;
        next.parent = head.version;
        next.time = Utc::now().timestamp_millis();
        next.author = actor.to_string();
        next.comment = comment;
        next.rollback_of = None;
        self.commit_config(next.clone()).await?;
        summary.version = next.version;
        Ok((next, summary))
    }

    /// Make a new version with an earlier version's contents (on the Overseer).
    pub async fn rollback_lattice_config(&self, actor: &str, version: u64, comment: Option<String>) -> Result<ConfigVersion> {
        self.ensure_writable()?;
        let _guard = self.lattice_config.write_lock.lock().await;
        let comment = comment_of(comment)?;
        let target = match self.stored_version(version).await? {
            Some(StoredVersion { body: Some(body), .. }) => body,
            Some(StoredVersion { problem, .. }) => {
                return Err(invalid(format!("Version {} failed verification ({}); choose another.", version, problem.unwrap_or_default())))
            }
            None => return Err(EngineError::NotFound(format!("Configuration version {} not found.", version)).into()),
        };
        // The newest version may be the damaged one being rolled back from.
        let newest = self.stored_versions(1).await?.into_iter().next().map_or(0, |s| s.version);
        let next = ConfigVersion {
            format: FORMAT,
            version: newest + 1,
            parent: newest,
            time: Utc::now().timestamp_millis(),
            author: actor.to_string(),
            comment,
            rollback_of: Some(version),
            lattice: without_retired(target.lattice),
            hexes: BTreeMap::new(),
        };
        self.commit_config(next.clone()).await?;
        Ok(next)
    }

    /// Versions, newest first, with what each changed from its parent.
    pub async fn lattice_config_history(&self, limit: usize, offset: usize) -> Result<(Vec<Value>, usize)> {
        let all = self.stored_versions(100_000).await?;
        let total = all.len();
        let by_version: BTreeMap<u64, &ConfigVersion> = all.iter().filter_map(|s| s.body.as_ref().map(|b| (s.version, b))).collect();
        let empty = ConfigVersion::default();
        let entries = all
            .iter()
            .skip(offset)
            .take(limit)
            .map(|stored| match &stored.body {
                Some(body) => json!({
                    "version": body.version,
                    "parent": body.parent,
                    "time": body.time,
                    "author": body.author,
                    "comment": body.comment,
                    "rollback_of": body.rollback_of,
                    "sha256": stored.sha256,
                    "valid": true,
                    "diff": version_diff(by_version.get(&body.parent).copied().unwrap_or(&empty), body),
                }),
                None => json!({ "version": stored.version, "sha256": stored.sha256, "valid": false, "problem": stored.problem }),
            })
            .collect();
        Ok((entries, total))
    }

    /// One version for display (no secret values), and whether it checks out.
    pub async fn lattice_config_version(&self, version: u64) -> Result<Value> {
        match self.stored_version(version).await? {
            Some(StoredVersion { body: Some(body), .. }) => {
                let mut view = version_view(&body);
                view["valid"] = json!(true);
                Ok(view)
            }
            Some(StoredVersion { version, sha256, problem, .. }) => Ok(json!({ "version": version, "sha256": sha256, "valid": false, "problem": problem })),
            None => Err(EngineError::NotFound(format!("Configuration version {} not found.", version)).into()),
        }
    }

    /// The newest version for display, with the schema and every hex's state.
    pub async fn lattice_config_view(&self) -> Result<Value> {
        let newest = self.stored_versions(1).await?.into_iter().next();
        let body = newest.as_ref().and_then(|s| s.body.clone()).unwrap_or_default();
        let schema: Vec<Value> = CONFIG
            .iter()
            .map(|d| {
                json!({
                    "key": d.key,
                    "kind": d.kind,
                    "description": d.description,
                    "default": d.default_value(),
                    "secret": d.is_secret(),
                })
            })
            .collect();
        let mut hexes = Vec::new();
        let me = self.lattice_config();
        hexes.push(json!({ "name": self.name, "id": self.id.to_string(), "role": self.role(), "status": "active", "is_self": true, "config": me.status_json() }));
        for peer in self.peers.lock().await.iter() {
            hexes.push(json!({
                "name": peer.hex.name,
                "id": peer.hex.id,
                "role": peer.hex.role,
                "status": peer.status,
                "is_self": false,
                "config": { "version": peer.hex.config_version, "sha256": peer.hex.config_sha256, "state": peer.hex.config_state },
            }));
        }
        Ok(json!({
            "version": body.version,
            "sha256": newest.as_ref().map(|s| s.sha256.clone()).unwrap_or_default(),
            "valid": newest.as_ref().is_none_or(|s| s.body.is_some()),
            "problem": newest.as_ref().and_then(|s| s.problem.clone()),
            "time": body.time,
            "author": body.author,
            "comment": body.comment,
            "lattice": scope_view(&without_retired(body.lattice.clone())),
            "schema": schema,
            "hexes": hexes,
        }))
    }
}

/// Apply new configuration versions as they arrive (every hex).
pub fn spawn_config_task(engine: std::sync::Arc<HexDBEngine>, shutdown_rx: watch::Receiver<()>) {
    crate::tasks::supervise("configuration", shutdown_rx.clone(), move || {
        let (engine, mut shutdown_rx) = (engine.clone(), shutdown_rx.clone());
        async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        debug!("🛑 Configuration task is shutting down...");
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(1)) => engine.refresh_lattice_config().await,
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_defaults_validate() {
        for d in CONFIG.iter().filter(|d| !d.is_secret()) {
            assert_eq!(validate(d, &d.default_value()).unwrap(), d.default_value(), "{}", d.key);
        }
        assert!(definition("file_storage.backend").is_none(), "remote storage settings were removed");
    }

    #[test]
    fn origins_are_normalized_as_browsers_send_them() {
        let cors = definition("cors.allowed_origins").unwrap();
        let ok = |v: &str| validate(cors, &json!(v)).unwrap();
        assert_eq!(ok(""), json!(""));
        assert_eq!(ok("*"), json!("*"));
        assert_eq!(ok("HTTPS://App.Example.com/, http://localhost:5173 https://app.example.com:443"), json!("https://app.example.com, http://localhost:5173"));
        assert_eq!(ok("http://[::1]:8080,http://127.0.0.1"), json!("http://[::1]:8080, http://127.0.0.1"));
        for bad in ["app.example.com", "https://app.example.com/path", "ftp://x.com", "https://user@x.com", "https://x.com:99999", "*, https://x.com", "https://[zz]", "https://a..b"] {
            assert!(validate(cors, &json!(bad)).is_err(), "{}", bad);
        }
    }

    #[test]
    fn validation_normalizes_and_never_echoes_values() {
        let set = definition("ai.tagging.send").unwrap();
        assert_eq!(validate(set, &json!(["image", "metadata", "image"])).unwrap(), json!(["metadata", "image"]));
        assert!(validate(set, &json!(["audio"])).is_err());
        let url = definition("ai.base_url").unwrap();
        assert_eq!(validate(url, &json!("https://example.com/v1/")).unwrap(), json!("https://example.com/v1"));
        assert!(validate(url, &json!("file:///etc/passwd")).is_err());
        for metadata in ["http://169.254.169.254/latest/meta-data", "http://metadata.google.internal/computeMetadata/v1", "http://user@169.254.169.254:80/", "http://[fd00:ec2::254]/"] {
            assert!(validate(url, &json!(metadata)).is_err(), "{}", metadata);
        }
        assert!(validate(url, &json!("http://localhost:11434/v1")).is_ok(), "a local model server is fine");
        let budget = definition("ai.monthly_token_budget").unwrap();
        assert!(validate(budget, &json!(-1)).is_err());
        let key = definition("ai.api_key").unwrap();
        let err = validate(key, &json!("x".repeat(MAX_SECRET + 1))).unwrap_err().to_string();
        assert!(!err.contains("xxxx"));
        assert!(validate(key, &json!("")).is_err());
    }

    #[test]
    fn checksums_detect_damage() {
        let body = ConfigVersion { format: FORMAT, version: 3, parent: 2, author: "admin".into(), ..Default::default() };
        let (text, sha) = body.canonical();
        let good = parse_stored(&json!({ "version": 3, "sha256": sha, "body": text }));
        assert_eq!(good.body.as_ref(), Some(&body));
        let damaged = parse_stored(&json!({ "version": 3, "sha256": sha, "body": text.replace("admin", "eve") }));
        assert!(damaged.body.is_none() && damaged.problem.unwrap().contains("checksum"));
        let renumbered = parse_stored(&json!({ "version": 4, "sha256": sha, "body": text }));
        assert!(renumbered.body.is_none());
        assert_eq!(body.canonical(), body.clone().canonical(), "canonical form is stable");
    }

    #[test]
    fn diffs_name_changed_and_removed_keys() {
        let mut old = ConfigVersion::default();
        old.lattice.settings.insert("ai.provider".into(), json!("openai"));
        old.lattice.settings.insert("ai.model".into(), json!("a"));
        let mut new = old.clone();
        new.lattice.settings.insert("ai.provider".into(), json!("anthropic"));
        new.lattice.settings.remove("ai.model");
        new.hexes.entry("Tosa".into()).or_default().settings.insert("file_storage.backend".into(), json!("s3"));
        let diff = version_diff(&old, &new);
        assert_eq!(diff["changed"], json!(["ai.provider", "hex:Tosa/file_storage.backend"]), "older versions' overrides still show in history");
        assert_eq!(diff["removed"], json!(["ai.model"]));
    }

    /// Store a version document directly, as a damaged or newer copy would arrive.
    async fn store_raw(engine: &HexDBEngine, version: u64, body: &str, sha256: &str) {
        let doc = json!({ "version": version, "time": 1, "author": "x", "sha256": sha256, "body": body });
        engine.put_system_document(CONFIG_TESSELLATION, doc_id(version), doc, None).await.unwrap();
    }

    #[tokio::test]
    async fn a_hex_rejects_versions_that_dont_check_out_and_keeps_the_last_good_one() {
        let (engine, dir) = crate::engine::test_engine().await;
        let change = |changes: Value| ConfigChange { changes: changes.as_object().unwrap().clone(), ..Default::default() };
        engine.update_lattice_config("admin", change(json!({ "ai.provider": "anthropic", "ai.api_key": "sk-1" }))).await.unwrap();
        let good = engine.lattice_config();
        assert_eq!((good.version, good.state()), (1, "applied"));
        assert_eq!(engine.config_secret("ai.api_key").unwrap().expose(), "sk-1");

        // Version 2 arrives damaged: its body doesn't match its checksum.
        let mut next = engine.head().await.unwrap();
        next.version = 2;
        next.parent = 1;
        next.lattice.settings.insert("ai.provider".into(), json!("openai"));
        let (body, sha) = next.canonical();
        store_raw(&engine, 2, &body.replace("openai", "evilco"), &sha).await;
        engine.refresh_lattice_config().await;
        let now = engine.lattice_config();
        assert_eq!((now.version, now.state(), now.rejected.as_ref().map(|r| r.0)), (1, "rejected", Some(2)));
        assert!(now.rejected.as_ref().unwrap().1.contains("checksum"), "{:?}", now.rejected);
        assert_eq!(engine.config_value("ai.provider"), json!("anthropic"), "the last good version stays in effect");
        // New changes are refused until it's rolled back.
        assert!(engine.update_lattice_config("admin", change(json!({ "ai.model": "m" }))).await.is_err());

        // Version 3 checks out but names a setting this build doesn't know (a newer HexDB wrote it).
        let mut newer = next.clone();
        newer.version = 3;
        newer.parent = 2;
        newer.lattice.settings.insert("ai.future_feature".into(), json!(true));
        let (body, sha) = newer.canonical();
        store_raw(&engine, 3, &body, &sha).await;
        engine.refresh_lattice_config().await;
        let now = engine.lattice_config();
        assert_eq!((now.version, now.rejected.as_ref().map(|r| r.0)), (1, Some(3)));
        assert!(now.rejected.as_ref().unwrap().1.contains("unknown setting"), "{:?}", now.rejected);

        // Rolling back to version 1 recovers.
        let restored = engine.rollback_lattice_config("admin", 1, None).await.unwrap();
        assert_eq!(restored.version, 4);
        let now = engine.lattice_config();
        assert_eq!((now.version, now.state()), (4, "applied"));
        assert_eq!(engine.config_secret("ai.api_key").unwrap().expose(), "sk-1");
        engine.shutdown().await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn versions_with_retired_storage_settings_still_apply() {
        let (engine, dir) = crate::engine::test_engine().await;
        // A 1.6 preview version: remote storage settings, a storage secret and a per-hex override.
        let mut old = ConfigVersion { format: FORMAT, version: 1, author: "admin".into(), ..Default::default() };
        old.lattice.settings.insert("ai.provider".into(), json!("anthropic"));
        old.lattice.settings.insert("file_storage.backend".into(), json!("s3"));
        old.lattice.secrets.insert("file_storage.s3.secret_access_key".into(), seal(&engine, "file_storage.s3.secret_access_key", "abc", "admin"));
        old.hexes.entry("Tosa".into()).or_default().settings.insert("file_storage.backend".into(), json!("azure"));
        let (body, sha) = old.canonical();
        store_raw(&engine, 1, &body, &sha).await;
        engine.refresh_lattice_config().await;
        let now = engine.lattice_config();
        assert_eq!((now.version, now.state()), (1, "applied"), "{:?}", now.rejected);
        assert_eq!(engine.config_value("ai.provider"), json!("anthropic"));
        assert!(engine.config_value("file_storage.backend").is_null());
        // The next version drops them.
        let change = ConfigChange { changes: json!({ "ai.model": "m" }).as_object().unwrap().clone(), ..Default::default() };
        let (next, _) = engine.update_lattice_config("admin", change).await.unwrap();
        assert!(next.hexes.is_empty());
        assert!(next.lattice.settings.keys().chain(next.lattice.secrets.keys()).all(|k| !k.starts_with("file_storage.")), "{:?}", next.lattice);
        assert_eq!(next.lattice.settings["ai.provider"], json!("anthropic"));
        engine.shutdown().await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn secrets_debug_redacted() {
        assert_eq!(format!("{:?}", Secret("hunter2".into())), "Secret(..)");
    }
}
