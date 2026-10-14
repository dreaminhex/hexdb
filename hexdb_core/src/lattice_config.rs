// HexDB Core Lattice Configuration
//
// Settings that belong to the whole lattice rather than one machine: where
// file content is stored, which AI provider to use and what AI features may
// do. Admins change them through `PUT /config` (the admin UI and `hexdb
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
// Secrets (API keys, storage credentials) are sealed with the lattice transfer
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
// Settings under `file_storage.` can also be overridden for one hex (e.g. a
// hex in another region using a nearer bucket). Every other setting is
// lattice-wide.

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
/// Settings that may be overridden for one hex.
pub const PER_HEX_PREFIX: &str = "file_storage.";
const MAX_TEXT: usize = 2048;
const MAX_SECRET: usize = 16 * 1024;
const MAX_COMMENT: usize = 500;
const MAX_HEX_NAME: usize = 64;

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

    /// May be overridden for one hex.
    pub fn per_hex(&self) -> bool {
        self.key.starts_with(PER_HEX_PREFIX)
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

pub const BACKENDS: &[&str] = &["local", "s3", "azure", "gdrive", "dropbox"];
pub const AI_PROVIDERS: &[&str] = &["none", "anthropic", "openai", "github", "azure_openai", "openai_compatible"];
pub const AI_INPUTS: &[&str] = &["metadata", "text", "thumbnail", "image"];

pub const CONFIG: &[ConfigDef] = &[
    def("file_storage.backend", ConfigKind::Choice { choices: BACKENDS }, "\"local\"", "Where file content is stored: this hex's disks (local), Amazon S3 or an S3-compatible service, Azure Blob Storage, Google Drive or Dropbox. Content is encrypted before it leaves the hex."),
    text("file_storage.prefix", "Folder or key prefix inside the bucket, container or drive folder."),
    text("file_storage.s3.bucket", "S3 bucket name."),
    text("file_storage.s3.region", "S3 region, e.g. us-east-1 (some S3-compatible services use auto)."),
    def("file_storage.s3.endpoint", ConfigKind::Url, "\"\"", "Endpoint of an S3-compatible service (Cloudflare R2, Backblaze B2, Wasabi, MinIO); empty for Amazon S3."),
    text("file_storage.s3.access_key_id", "S3 access key ID."),
    secret("file_storage.s3.secret_access_key", "S3 secret access key."),
    text("file_storage.azure.account", "Azure storage account name."),
    text("file_storage.azure.container", "Azure Blob container name."),
    secret("file_storage.azure.account_key", "Azure storage account key."),
    text("file_storage.gdrive.folder_id", "ID of the Google Drive folder that holds HexDB's files."),
    text("file_storage.gdrive.client_id", "OAuth client ID of your Google Cloud app."),
    secret("file_storage.gdrive.client_secret", "OAuth client secret of your Google Cloud app."),
    secret("file_storage.gdrive.refresh_token", "OAuth refresh token for the Google account."),
    text("file_storage.dropbox.folder", "Dropbox folder that holds HexDB's files, e.g. /HexDB."),
    text("file_storage.dropbox.app_key", "Dropbox app key."),
    secret("file_storage.dropbox.app_secret", "Dropbox app secret."),
    secret("file_storage.dropbox.refresh_token", "OAuth refresh token for the Dropbox account."),
    def("ai.provider", ConfigKind::Choice { choices: AI_PROVIDERS }, "\"none\"", "AI provider for AI features: Anthropic (Claude), OpenAI, GitHub Models, Azure OpenAI, or any OpenAI-compatible endpoint (e.g. Ollama). Calls use this lattice's own key."),
    text("ai.model", "Model name, e.g. the provider's model ID (empty: the provider's default)."),
    def("ai.base_url", ConfigKind::Url, "\"\"", "API base URL (empty: the provider's public API). Required for Azure OpenAI and OpenAI-compatible endpoints."),
    secret("ai.api_key", "API key for the AI provider."),
    def("ai.monthly_token_budget", ConfigKind::Integer { min: 0, max: 1_000_000_000_000 }, "1000000", "Most tokens AI features may use per calendar month; they pause when it is reached (0: no limit)."),
    def("ai.max_tokens_per_file", ConfigKind::Integer { min: 100, max: 1_000_000 }, "4000", "Most tokens one file may use (input and output)."),
    def("ai.tagging.enabled", ConfigKind::Boolean, "false", "Let the AI add tags and a description to files as they are uploaded."),
    def("ai.tagging.send", ConfigKind::Set { choices: AI_INPUTS }, "[\"metadata\",\"text\"]", "What auto-tagging sends to the AI: extracted metadata, extracted text, the thumbnail, the full image."),
    def("ai.video_thumbnails.enabled", ConfigKind::Boolean, "false", "Let the AI pick the most representative frame for video thumbnails."),
    def("ai.video_thumbnails.frames", ConfigKind::Integer { min: 2, max: 16 }, "6", "Candidate frames the AI chooses from."),
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
            }
            Ok(Value::String(s.trim_end_matches('/').to_string()))
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

fn validate_hex_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > MAX_HEX_NAME || name.chars().any(char::is_control) {
        return Err(invalid(format!("Hex names are 1-{} characters.", MAX_HEX_NAME)));
    }
    Ok(())
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

/// Settings for one scope (the lattice, or one hex's overrides).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigScope {
    #[serde(default)]
    pub settings: BTreeMap<String, Value>,
    #[serde(default)]
    pub secrets: BTreeMap<String, SealedSecret>,
}

impl ConfigScope {
    fn is_empty(&self) -> bool {
        self.settings.is_empty() && self.secrets.is_empty()
    }
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
    /// Per-hex overrides (storage settings only), by hex name.
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
    /// Settings this hex overrides.
    pub overridden: Vec<String>,
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
            "overridden": self.overridden,
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

/// Check a version and work out what it means for the hex named `hex`.
fn resolve(engine: &HexDBEngine, body: &ConfigVersion, hex: &str) -> Result<AppliedConfig> {
    if body.format > FORMAT {
        bail!("written by a newer HexDB (format {})", body.format);
    }
    let check_scope = |scope: &ConfigScope, per_hex: bool| -> Result<()> {
        for (key, value) in &scope.settings {
            let def = definition(key).ok_or_else(|| anyhow!("unknown setting {} (written by a newer HexDB?)", key))?;
            if def.is_secret() || (per_hex && !def.per_hex()) {
                bail!("{} isn't allowed here", key);
            }
            validate(def, value).map_err(|e| anyhow!("{}", e))?;
        }
        for key in scope.secrets.keys() {
            let def = definition(key).ok_or_else(|| anyhow!("unknown secret {} (written by a newer HexDB?)", key))?;
            if !def.is_secret() || (per_hex && !def.per_hex()) {
                bail!("{} isn't allowed here", key);
            }
        }
        Ok(())
    };
    check_scope(&body.lattice, false)?;
    for (name, scope) in &body.hexes {
        check_scope(scope, true).map_err(|e| anyhow!("hex {}: {}", name, e))?;
    }
    let mut applied = AppliedConfig::defaults();
    // Every secret must open, not only this hex's, so a bad version is caught everywhere.
    let mut lattice_secrets = BTreeMap::new();
    for (key, secret) in &body.lattice.secrets {
        lattice_secrets.insert(key.clone(), open(engine, key, secret)?);
    }
    let mut hex_secrets = BTreeMap::new();
    for (name, scope) in &body.hexes {
        for (key, secret) in &scope.secrets {
            let value = open(engine, key, secret).map_err(|e| anyhow!("hex {}: {}", name, e))?;
            if name == hex {
                hex_secrets.insert(key.clone(), value);
            }
        }
    }
    for (key, value) in &body.lattice.settings {
        applied.values.insert(key.clone(), value.clone());
    }
    applied.secrets = lattice_secrets;
    if let Some(scope) = body.hexes.get(hex) {
        // An override replaces the whole storage configuration, so a hex never
        // mixes its own backend with the lattice's credentials.
        let overrides_storage = !scope.is_empty();
        if overrides_storage {
            for def in CONFIG.iter().filter(|d| d.per_hex()) {
                if def.is_secret() {
                    applied.secrets.remove(def.key);
                } else {
                    applied.values.insert(def.key.to_string(), def.default_value());
                }
            }
        }
        for (key, value) in &scope.settings {
            applied.values.insert(key.clone(), value.clone());
            applied.overridden.push(key.clone());
        }
        for (key, value) in hex_secrets {
            applied.overridden.push(key.clone());
            applied.secrets.insert(key, value);
        }
        applied.overridden.sort();
    }
    applied.version = body.version;
    applied.sha256 = body.canonical().1;
    Ok(applied)
}

// ---------------------------------------------------------------------------
// Changes
// ---------------------------------------------------------------------------

/// A change request: `changes` maps setting keys to new values (`null` clears).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigChange {
    pub changes: Map<String, Value>,
    /// Change this hex's overrides instead of the lattice's (storage settings only).
    #[serde(default)]
    pub hex: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hex: Option<String>,
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
            Some(body) => resolve(self, body, &self.name),
            None => Err(anyhow!("{}", stored.problem.clone().unwrap_or_default())),
        };
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

    /// Store a new version on the Overseer and apply it.
    async fn commit_config(&self, body: ConfigVersion) -> Result<()> {
        let (text, sha) = body.canonical();
        let doc = json!({ "version": body.version, "time": body.time, "author": body.author, "sha256": sha, "body": text });
        // Check it as every hex will before anyone sees it.
        resolve(self, &body, &self.name).map_err(|e| invalid(format!("{:#}", e)))?;
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
        let hex = match request.hex.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
            Some(name) => {
                validate_hex_name(name)?;
                Some(name.to_string())
            }
            None => None,
        };
        let mut next = head.clone();
        let scope = match &hex {
            Some(name) => next.hexes.entry(name.clone()).or_default(),
            None => &mut next.lattice,
        };
        let mut summary = ChangeSummary { hex: hex.clone(), ..Default::default() };
        for (key, value) in &request.changes {
            let def = definition(key).ok_or_else(|| invalid(format!("Unknown setting '{}'.", key)))?;
            if hex.is_some() && !def.per_hex() {
                return Err(invalid(format!("Only {}* settings can be set for one hex; '{}' applies to the whole lattice.", PER_HEX_PREFIX, key)));
            }
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
        if let Some(name) = &hex {
            if next.hexes.get(name).is_some_and(ConfigScope::is_empty) {
                next.hexes.remove(name);
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
            lattice: target.lattice,
            hexes: target.hexes,
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
                    "per_hex": d.per_hex(),
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
            "lattice": scope_view(&body.lattice),
            "overrides": body.hexes.iter().map(|(name, scope)| (name.clone(), scope_view(scope))).collect::<Map<String, Value>>(),
            "schema": schema,
            "hexes": hexes,
        }))
    }
}

/// Apply new configuration versions as they arrive (every hex).
pub fn spawn_config_task(engine: std::sync::Arc<HexDBEngine>, mut shutdown_rx: watch::Receiver<()>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    debug!("🛑 Configuration task is shutting down...");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => engine.refresh_lattice_config().await,
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
        assert!(definition("file_storage.backend").unwrap().per_hex());
        assert!(!definition("ai.provider").unwrap().per_hex());
    }

    #[test]
    fn validation_normalizes_and_never_echoes_values() {
        let set = definition("ai.tagging.send").unwrap();
        assert_eq!(validate(set, &json!(["image", "metadata", "image"])).unwrap(), json!(["metadata", "image"]));
        assert!(validate(set, &json!(["video"])).is_err());
        let url = definition("ai.base_url").unwrap();
        assert_eq!(validate(url, &json!("https://example.com/v1/")).unwrap(), json!("https://example.com/v1"));
        assert!(validate(url, &json!("file:///etc/passwd")).is_err());
        let budget = definition("ai.monthly_token_budget").unwrap();
        assert!(validate(budget, &json!(-1)).is_err());
        let key = definition("ai.api_key").unwrap();
        let err = validate(key, &json!("x".repeat(MAX_SECRET + 1))).unwrap_err().to_string();
        assert!(!err.contains("xxxx"));
        assert!(validate(key, &json!("")).is_err());
        assert!(validate(definition("file_storage.backend").unwrap(), &json!("ftp")).is_err());
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
        assert_eq!(diff["changed"], json!(["ai.provider", "hex:Tosa/file_storage.backend"]));
        assert_eq!(diff["removed"], json!(["ai.model"]));
    }

    #[test]
    fn secrets_debug_redacted() {
        assert_eq!(format!("{:?}", Secret("hunter2".into())), "Secret(..)");
    }
}
