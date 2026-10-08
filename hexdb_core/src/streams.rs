// HexDB Core Streams (publish/subscribe)
//
// A stream is a named, ordered log of messages, kept for `retention_hours`.
// Producers publish messages; consumers read them in order by offset (the
// message ID, a ULID that sorts by time), either tracking the offset
// themselves or as a consumer group whose committed offset HexDB keeps, or
// live as Server-Sent Events.
//
// Streams can also be wired up without code:
//
//   sources       messages from a tessellation's committed changes
//                 (optionally filtered, and only some operations), e.g. every
//                 new order;
//   destinations  messages POSTed in batches to a webhook, each destination
//                 with its own offset, retried until delivered.
//
// Messages are documents in the system tessellation `_stream_<name>` (so they
// are encrypted, replicated, and expire by TTL); configurations live in
// `_streams` and consumer offsets in `_stream_offsets`. Sources and
// destinations run on the Overseer. Permissions use the name `stream:<name>`
// in role grants: read to consume, write to publish, manage to configure.

use crate::{
    changes::ChangeKind,
    engine::{ChangeReader, EngineError, HexDBEngine},
    filter::Filter,
};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};
use ulid::Ulid;

/// System tessellation of stream configurations.
pub const STREAMS_TESSELLATION: &str = "_streams";
/// System tessellation of consumer group offsets.
pub const STREAM_OFFSETS_TESSELLATION: &str = "_stream_offsets";
/// Most messages in one publish or read.
pub const MAX_BATCH: usize = 1000;

/// The tessellation holding a stream's messages.
pub fn messages_tessellation(stream: &str) -> String {
    format!("_stream_{}", stream)
}

/// The permission resource name of a stream.
pub fn resource(stream: &str) -> String {
    format!("stream:{}", stream)
}

/// Changes that feed a stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamSource {
    pub tessellation: String,
    /// Only changes whose document matches (puts only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Value>,
    /// "put", "delete" (default both).
    #[serde(default)]
    pub ops: Vec<String>,
}

/// Where a stream's messages are delivered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamDestination {
    /// "webhook".
    #[serde(default = "webhook")]
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "default_batch")]
    pub batch_size: usize,
}

fn webhook() -> String {
    "webhook".into()
}

fn default_batch() -> usize {
    100
}

fn default_retention() -> u64 {
    24 * 7
}

/// A stream's configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Hours a message is kept (0: forever).
    #[serde(default = "default_retention")]
    pub retention_hours: u64,
    #[serde(default)]
    pub sources: Vec<StreamSource>,
    #[serde(default)]
    pub destinations: Vec<StreamDestination>,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub created_by: String,
}

/// A message to publish.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewMessage {
    pub payload: Value,
    /// Optional partitioning or deduplication key.
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    EngineError::Invalid(message.into()).into()
}

fn doc_id(text: &str) -> Ulid {
    let hash = blake3::derive_key("HexDB 2026 stream id v1", text.as_bytes());
    Ulid::from(u128::from_be_bytes(hash[..16].try_into().unwrap()))
}

fn offset_id(stream: &str, group: &str) -> Ulid {
    doc_id(&format!("{}\u{0}{}", stream, group))
}

impl StreamConfig {
    fn validate(&self) -> Result<()> {
        crate::catalog::validate_tessellation_name(&self.name).map_err(|e| invalid(e.to_string().replace("Tessellation", "Stream")))?;
        if self.description.len() > 500 {
            bail!(invalid("description must be at most 500 characters."));
        }
        for (i, source) in self.sources.iter().enumerate() {
            crate::catalog::validate_tessellation_name(&source.tessellation).map_err(|e| invalid(format!("sources[{}]: {}", i, e)))?;
            if let Some(filter) = &source.filter {
                Filter::parse(filter).map_err(|e| invalid(format!("sources[{}].filter: {}", i, e)))?;
            }
            if let Some(op) = source.ops.iter().find(|o| !matches!(o.as_str(), "put" | "delete")) {
                bail!(invalid(format!("sources[{}].ops: '{}' isn't put or delete.", i, op)));
            }
        }
        for (i, d) in self.destinations.iter().enumerate() {
            if d.kind != "webhook" {
                bail!(invalid(format!("destinations[{}]: kind must be webhook.", i)));
            }
            let url = reqwest::Url::parse(&d.url).map_err(|_| invalid(format!("destinations[{}]: '{}' isn't a URL.", i, d.url)))?;
            if !matches!(url.scheme(), "http" | "https") {
                bail!(invalid(format!("destinations[{}]: the URL must be http or https.", i)));
            }
            if d.batch_size == 0 || d.batch_size > MAX_BATCH {
                bail!(invalid(format!("destinations[{}]: batch_size must be 1-{}.", i, MAX_BATCH)));
            }
        }
        Ok(())
    }
}

/// A delivery task's state, for `GET /streams/{name}`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TaskStatus {
    pub delivered: u64,
    pub last_error: Option<String>,
}

/// Running sources and destinations.
#[derive(Default)]
pub struct StreamTasks {
    running: std::sync::Mutex<HashMap<String, (String, tokio::task::JoinHandle<()>)>>,
    pub status: std::sync::Mutex<HashMap<String, TaskStatus>>,
}

impl StreamTasks {
    fn update(&self, key: &str, f: impl FnOnce(&mut TaskStatus)) {
        f(self.status.lock().unwrap().entry(key.to_string()).or_default());
    }
}

/// A message as returned by the API.
pub fn message_json(doc: &crate::document::Document) -> Value {
    let mut value = doc.data_json();
    if let Value::Object(map) = &mut value {
        map.insert("offset".into(), Value::String(doc.id.to_string()));
        map.insert("time".into(), json!(doc.id.timestamp_ms()));
    }
    value
}

impl HexDBEngine {
    /// Every stream.
    pub async fn list_streams(&self) -> Result<Vec<StreamConfig>> {
        if !self.tessellation_exists(STREAMS_TESSELLATION) {
            return Ok(Vec::new());
        }
        let page = self.list_documents(STREAMS_TESSELLATION, None, usize::MAX).await?;
        let mut list: Vec<StreamConfig> = page.documents.iter().filter_map(|d| serde_json::from_value(d.data_json()).ok()).collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(list)
    }

    pub async fn get_stream(&self, name: &str) -> Result<Option<StreamConfig>> {
        let Some(doc) = self.get_system_document(STREAMS_TESSELLATION, doc_id(name)).await? else { return Ok(None) };
        Ok(serde_json::from_value(doc.data_json()).ok())
    }

    /// Create or replace a stream's configuration.
    pub async fn save_stream(&self, mut config: StreamConfig, by: &str, create: bool) -> Result<StreamConfig> {
        config.validate()?;
        let existing = self.get_stream(&config.name).await?;
        match (&existing, create) {
            (Some(_), true) => bail!(EngineError::Conflict(format!("A stream named '{}' already exists.", config.name))),
            (None, false) => bail!(EngineError::NotFound(format!("Stream '{}' not found.", config.name))),
            (Some(old), false) => {
                config.created = old.created;
                config.created_by = old.created_by.clone();
            }
            (None, true) => {
                config.created = chrono::Utc::now().timestamp_millis();
                config.created_by = by.to_string();
            }
        }
        self.ensure_system_tessellation(&messages_tessellation(&config.name))?;
        // Sources start from this moment, not from when their task gets going.
        let now = self.changes.published_seq();
        for i in 0..config.sources.len() {
            let group = format!("__source:{}", i);
            if self.get_system_document(STREAM_OFFSETS_TESSELLATION, offset_id(&config.name, &group)).await?.is_none() {
                let data = json!({ "stream": config.name, "group": group, "history_id": self.history_id(), "seq": now, "updated": chrono::Utc::now().timestamp_millis() });
                self.put_system_document(STREAM_OFFSETS_TESSELLATION, offset_id(&config.name, &group), data, None).await?;
            }
        }
        self.put_system_document(STREAMS_TESSELLATION, doc_id(&config.name), serde_json::to_value(&config)?, None).await?;
        Ok(config)
    }

    /// Delete a stream, its messages and its offsets.
    pub async fn delete_stream(&self, name: &str) -> Result<bool> {
        if self.get_stream(name).await?.is_none() {
            return Ok(false);
        }
        self.ensure_writable()?;
        self.delete_system_document(STREAMS_TESSELLATION, doc_id(name)).await?;
        self.delete_tessellation_unchecked(&messages_tessellation(name)).await?;
        if self.tessellation_exists(STREAM_OFFSETS_TESSELLATION) {
            let page = self.list_documents(STREAM_OFFSETS_TESSELLATION, None, usize::MAX).await?;
            for doc in page.documents.iter().filter(|d| d.data_json()["stream"] == name) {
                self.delete_system_document(STREAM_OFFSETS_TESSELLATION, doc.id).await?;
            }
        }
        Ok(true)
    }

    async fn stream_or_404(&self, name: &str) -> Result<StreamConfig> {
        self.get_stream(name).await?.ok_or_else(|| EngineError::NotFound(format!("Stream '{}' not found.", name)).into())
    }

    /// Publish messages; returns their offsets.
    pub async fn publish(&self, name: &str, messages: Vec<NewMessage>, by: &str) -> Result<Vec<String>> {
        let config = self.stream_or_404(name).await?;
        if messages.is_empty() || messages.len() > MAX_BATCH {
            bail!(invalid(format!("Publish 1-{} messages at a time.", MAX_BATCH)));
        }
        let ttl = (config.retention_hours > 0).then(|| chrono::Utc::now().timestamp_millis() + config.retention_hours as i64 * 3_600_000);
        let tess = messages_tessellation(name);
        let mut ids = Vec::with_capacity(messages.len());
        let mut docs = Vec::with_capacity(messages.len());
        {
            // Offsets must increase even within one millisecond.
            let mut generator = self.stream_ids.lock().unwrap();
            for m in messages {
                let id = generator.generate().unwrap_or_else(|_| Ulid::new());
                let data = json!({ "payload": m.payload, "key": m.key, "headers": m.headers, "published_by": by });
                docs.push(crate::document::Document { id, tessellation: tess.clone(), data: crate::document::infer_fields_from_json(&data), ttl });
                ids.push(id.to_string());
            }
        }
        self.put_system_documents(&tess, docs).await?;
        Ok(ids)
    }

    /// Messages after an offset (from the start without one), oldest first.
    pub async fn read_stream(&self, name: &str, after: Option<Ulid>, limit: usize) -> Result<(Vec<Value>, Option<String>)> {
        self.stream_or_404(name).await?;
        let tess = messages_tessellation(name);
        if !self.tessellation_exists(&tess) {
            return Ok((Vec::new(), after.map(|a| a.to_string())));
        }
        let page = self.list_documents(&tess, after, limit.clamp(1, MAX_BATCH)).await?;
        let last = page.documents.last().map(|d| d.id.to_string()).or_else(|| after.map(|a| a.to_string()));
        Ok((page.documents.iter().map(message_json).collect(), last))
    }

    /// Wait (up to `timeout`) until a message is published after `after`.
    pub async fn wait_for_messages(&self, name: &str, after: Option<Ulid>, timeout: Duration) {
        let tess = messages_tessellation(name);
        let mut receiver = self.changes.subscribe();
        // Something may already be there.
        if let Ok(page) = self.list_documents(&tess, after, 1).await {
            if !page.documents.is_empty() {
                return;
            }
        }
        let _ = tokio::time::timeout(timeout, async {
            while let Ok(change) = receiver.recv().await {
                if change.tessellation == tess && change.kind == ChangeKind::Put {
                    return;
                }
            }
        })
        .await;
    }

    /// A consumer group's committed offset.
    pub async fn group_offset(&self, name: &str, group: &str) -> Result<Option<Ulid>> {
        let Some(doc) = self.get_system_document(STREAM_OFFSETS_TESSELLATION, offset_id(name, group)).await? else { return Ok(None) };
        Ok(doc.data_json()["offset"].as_str().and_then(|s| Ulid::from_string(s).ok()))
    }

    /// Commit a consumer group's offset (only forward).
    pub async fn commit_offset(&self, name: &str, group: &str, offset: Ulid) -> Result<()> {
        if group.is_empty() || group.len() > 100 {
            bail!(invalid("group must be 1-100 characters."));
        }
        let current = self.group_offset(name, group).await?;
        if current.is_some_and(|c| c >= offset) {
            return Ok(());
        }
        let data = json!({ "stream": name, "group": group, "offset": offset.to_string(), "updated": chrono::Utc::now().timestamp_millis() });
        self.put_system_document(STREAM_OFFSETS_TESSELLATION, offset_id(name, group), data, None).await
    }

    /// Consumer groups of a stream with their offsets and pending messages (up to 10,000 counted).
    pub async fn stream_groups(&self, name: &str) -> Result<Vec<Value>> {
        if !self.tessellation_exists(STREAM_OFFSETS_TESSELLATION) {
            return Ok(Vec::new());
        }
        let tess = messages_tessellation(name);
        let page = self.list_documents(STREAM_OFFSETS_TESSELLATION, None, usize::MAX).await?;
        let mut out = Vec::new();
        for doc in page.documents.iter().filter(|d| d.data_json()["stream"] == name) {
            let data = doc.data_json();
            let offset = data["offset"].as_str().and_then(|s| Ulid::from_string(s).ok());
            let pending = self.list_documents(&tess, offset, 10_000).await.map(|p| p.documents.len()).unwrap_or(0);
            out.push(json!({ "group": data["group"], "offset": data["offset"], "updated": data["updated"], "pending": pending }));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Sources and destinations
// ---------------------------------------------------------------------------

/// Keep each stream's sources and destinations running on the Overseer.
pub fn spawn_streams(engine: Arc<HexDBEngine>, mut shutdown_rx: watch::Receiver<()>) {
    tokio::spawn(async move {
        loop {
            reconcile(&engine).await;
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
        let mut running = engine.stream_tasks.running.lock().unwrap();
        for (_, (_, handle)) in running.drain() {
            handle.abort();
        }
    });
}

async fn reconcile(engine: &Arc<HexDBEngine>) {
    let wanted: Vec<(String, String, StreamConfig, usize, bool)> = if engine.is_writable() {
        match engine.list_streams().await {
            Ok(list) => list
                .into_iter()
                .flat_map(|s| {
                    let sources = (0..s.sources.len()).map(|i| (format!("{}/source/{}", s.name, i), serde_json::to_string(&s.sources[i]).unwrap_or_default(), s.clone(), i, true));
                    let dests = (0..s.destinations.len()).map(|i| (format!("{}/destination/{}", s.name, i), serde_json::to_string(&s.destinations[i]).unwrap_or_default(), s.clone(), i, false));
                    sources.chain(dests).collect::<Vec<_>>()
                })
                .collect(),
            Err(_) => return,
        }
    } else {
        Vec::new()
    };
    let mut running = engine.stream_tasks.running.lock().unwrap();
    // Stop tasks that are gone or changed.
    running.retain(|key, (fingerprint, handle)| {
        let keep = wanted.iter().any(|(k, f, ..)| k == key && f == fingerprint) && !handle.is_finished();
        if !keep {
            handle.abort();
        }
        keep
    });
    for (key, fingerprint, config, index, is_source) in wanted {
        if running.contains_key(&key) {
            continue;
        }
        let engine = engine.clone();
        let task_key = key.clone();
        let handle = if is_source {
            tokio::spawn(async move { run_source(engine, config, index, task_key).await })
        } else {
            tokio::spawn(async move { run_destination(engine, config, index, task_key).await })
        };
        running.insert(key, (fingerprint, handle));
    }
}

/// Where a source left off: (history ID, sequence).
async fn source_position(engine: &HexDBEngine, stream: &str, group: &str) -> u64 {
    let end = engine.changes.published_seq();
    let Ok(Some(doc)) = engine.get_system_document(STREAM_OFFSETS_TESSELLATION, offset_id(stream, group)).await else { return end };
    let data = doc.data_json();
    match (data["history_id"].as_str(), data["seq"].as_u64()) {
        (Some(h), Some(seq)) if h == engine.history_id() && seq >= engine.history_available_after() && seq <= end => seq,
        _ => end,
    }
}

async fn run_source(engine: Arc<HexDBEngine>, config: StreamConfig, index: usize, key: String) {
    let source = config.sources[index].clone();
    let group = format!("__source:{}", index);
    let filter = source.filter.as_ref().and_then(|f| Filter::parse(f).ok());
    let mut reader = ChangeReader::new(source_position(&engine, &config.name, &group).await);
    info!("📡 Stream '{}' follows changes to '{}'.", config.name, source.tessellation);
    let mut saved_at = std::time::Instant::now();
    let mut saved_cursor = reader.cursor;
    // The last change position that mattered to this source; its own
    // bookkeeping writes move the feed on too, but needn't be saved.
    let mut relevant = reader.cursor;
    let mut pending: Vec<NewMessage> = Vec::new();
    loop {
        let change = match tokio::time::timeout(Duration::from_millis(200), reader.next(&engine)).await {
            Ok(Some(change)) => Some(change),
            Ok(None) => return,
            Err(_) => None,
        };
        if let Some(change) = &change {
            let op = match change.kind {
                ChangeKind::Put => "put",
                ChangeKind::Delete => "delete",
                ChangeKind::DropTessellation => "",
            };
            let wanted_op = source.ops.is_empty() || source.ops.iter().any(|o| o == op);
            let matches = match (&filter, &change.document) {
                (Some(f), Some(doc)) => f.matches(doc),
                (Some(_), None) => false,
                (None, _) => true,
            };
            if !change.tessellation.starts_with('_') {
                relevant = change.seq;
            }
            if change.tessellation == source.tessellation && !op.is_empty() && wanted_op && matches {
                let mut headers = BTreeMap::new();
                headers.insert("op".to_string(), op.to_string());
                headers.insert("tessellation".to_string(), change.tessellation.clone());
                headers.insert("seq".to_string(), change.seq.to_string());
                let payload = change.document.as_ref().map(|d| d.to_api_json()).unwrap_or_else(|| json!({ "id": change.id.map(|i| i.to_string()) }));
                pending.push(NewMessage { payload, key: change.id.map(|i| i.to_string()), headers });
            }
        }
        // Publish what's collected when idle or full, then save the position.
        if !pending.is_empty() && (change.is_none() || pending.len() >= 500) {
            let count = pending.len() as u64;
            match engine.publish(&config.name, std::mem::take(&mut pending), &format!("source:{}", source.tessellation)).await {
                Ok(_) => engine.stream_tasks.update(&key, |s| {
                    s.delivered += count;
                    s.last_error = None;
                }),
                Err(e) => {
                    warn!("⚠️ Stream '{}' source stopped: {:#}", config.name, e);
                    engine.stream_tasks.update(&key, |s| s.last_error = Some(format!("{:#}", e)));
                    return;
                }
            }
        }
        // Save the position (after the last relevant change) at most once a second.
        if pending.is_empty() && relevant != saved_cursor && (change.is_none() || saved_at.elapsed() >= Duration::from_secs(1)) {
            let data = json!({ "stream": config.name, "group": group, "history_id": engine.history_id(), "seq": relevant, "updated": chrono::Utc::now().timestamp_millis() });
            if let Err(e) = engine.put_system_document(STREAM_OFFSETS_TESSELLATION, offset_id(&config.name, &group), data, None).await {
                debug!("Couldn't save stream source position: {:#}", e);
            }
            saved_cursor = relevant;
            saved_at = std::time::Instant::now();
        }
    }
}

async fn run_destination(engine: Arc<HexDBEngine>, config: StreamConfig, index: usize, key: String) {
    let destination = config.destinations[index].clone();
    let group = format!("__destination:{}", index);
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none()).build() {
        Ok(c) => c,
        Err(_) => return,
    };
    info!("📡 Stream '{}' delivers to {}.", config.name, destination.url);
    loop {
        let offset = engine.group_offset(&config.name, &group).await.ok().flatten();
        let (messages, last) = match engine.read_stream(&config.name, offset, destination.batch_size).await {
            Ok(page) => page,
            Err(_) => return,
        };
        if messages.is_empty() {
            engine.wait_for_messages(&config.name, offset, Duration::from_secs(5)).await;
            continue;
        }
        let mut request = client.post(&destination.url).json(&messages).header("x-hexdb-stream", &config.name);
        for (k, v) in &destination.headers {
            request = request.header(k, v);
        }
        let sent = match request.send().await {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) => Err(format!("{} returned {}", destination.url, r.status())),
            Err(e) => Err(format!("POST {}: {}", destination.url, e)),
        };
        match sent {
            Ok(()) => {
                let count = messages.len() as u64;
                if let Some(last) = last.and_then(|l| Ulid::from_string(&l).ok()) {
                    if let Err(e) = engine.commit_offset(&config.name, &group, last).await {
                        warn!("⚠️ Couldn't commit stream '{}' delivery offset: {:#}", config.name, e);
                    }
                }
                engine.stream_tasks.update(&key, |s| {
                    s.delivered += count;
                    s.last_error = None;
                });
            }
            Err(e) => {
                engine.stream_tasks.update(&key, |s| s.last_error = Some(e.clone()));
                debug!("Stream '{}' delivery failed: {}", config.name, e);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}
