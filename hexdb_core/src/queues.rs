// HexDB Core Queues (message delivery to competing consumers)
//
// Streams are logs: every consumer group reads every message. A queue hands
// each message to one consumer at a time:
//
//   send       messages go in, optionally delayed or de-duplicated by a key;
//   receive    a consumer leases up to N ready messages for the visibility
//              timeout, getting a receipt for each (long-polling up to 20 s);
//   ack        with its receipt, a consumer deletes a message it handled;
//   nack       or gives it back (optionally after a delay);
//   extend     or keeps it longer.
//
// A lease that runs out makes the message ready again. A message received
// `max_receives` times without an ack goes to the queue's `dead_letter` queue
// (or is dropped, logged, without one). Receipts are random per lease, so a
// consumer whose lease ran out can't ack a message someone else now holds.
//
// Messages are documents in the system tessellation `_queue_<name>` (so they
// are encrypted, replicated, and expire after `retention_hours`); queue
// configurations live in `_queues`. Every queue operation changes messages,
// so it runs on the Overseer (replicas forward them), serialized per queue,
// with an in-memory index of when each message is next ready, rebuilt from the
// documents when a hex becomes the Overseer. Permissions use `queue:<name>` in
// role grants: write to send, read to receive/ack/nack, manage to configure.
//
// The index never disagrees with the documents for long: a commit is visible
// in the hex before it's durable, and a request can go away (a timeout, a
// client gone) while it waits, after the write but before the index is
// updated. So the index counts as unloaded from just before each write until
// it's updated after it; an operation that never got there leaves it to be
// rebuilt from the documents by the next one. A lease is checked against the
// stored message before it's handed out, too.
//
// A move to the dead-letter queue is one commit (delete here, put there), so a
// message is always in exactly one of the two. Dead-letter queues can't form a
// cycle. De-duplication keys are documents in `_queue_dedupe` (replicated,
// expiring after 5 minutes), written in the same commit as the message, so
// they survive a restart or a new Overseer and only count once the message is
// stored.

use crate::engine::{EngineError, HexDBEngine};
use anyhow::{bail, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ulid::Ulid;

/// Queue configurations.
pub const QUEUES_TESSELLATION: &str = "_queues";
/// Most messages in one send or receive.
pub const MAX_BATCH: usize = 100;
/// Longest a receive waits for messages.
pub const MAX_WAIT_SECONDS: u64 = 20;
/// Largest message body (JSON), in bytes.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// How long a dead letter's arrival is kept while its move may still be committing.
const ARRIVAL_GRACE_MS: i64 = 60_000;
/// How long a de-duplication key is remembered.
const DEDUPE_MS: i64 = 5 * 60 * 1000;
/// De-duplication keys: `{"queue", "key", "message", "at"}`, expiring after `DEDUPE_MS`.
pub const DEDUPE_TESSELLATION: &str = "_queue_dedupe";

fn dedupe_id(queue: &str, key: &str) -> Ulid {
    let hash = blake3::derive_key("HexDB 2026 queue dedupe v1", format!("{}\u{0}{}", queue, key).as_bytes());
    Ulid::from(u128::from_be_bytes(hash[..16].try_into().unwrap()))
}

pub fn messages_tessellation(queue: &str) -> String {
    format!("_queue_{}", queue)
}

/// The permission resource name of a queue.
pub fn resource(queue: &str) -> String {
    format!("queue:{}", queue)
}

fn config_id(name: &str) -> Ulid {
    let hash = blake3::derive_key("HexDB 2026 queue id v1", name.as_bytes());
    Ulid::from(u128::from_be_bytes(hash[..16].try_into().unwrap()))
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    EngineError::Invalid(message.into()).into()
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// A queue's configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueConfig {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// How long a received message is hidden from other consumers.
    #[serde(default = "default_visibility")]
    pub visibility_seconds: u64,
    /// Receives before a message goes to the dead-letter queue.
    #[serde(default = "default_max_receives")]
    pub max_receives: u64,
    /// Where messages go after `max_receives` (a queue's name); none: dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dead_letter: Option<String>,
    /// How long a message is kept, delivered or not.
    #[serde(default = "default_retention")]
    pub retention_hours: u64,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub created_by: String,
}

fn default_visibility() -> u64 {
    30
}
fn default_max_receives() -> u64 {
    5
}
fn default_retention() -> u64 {
    96
}

impl QueueConfig {
    fn validate(&self) -> Result<()> {
        crate::catalog::validate_tessellation_name(&self.name).map_err(|e| invalid(e.to_string().replace("Tessellation", "Queue")))?;
        if self.description.len() > 500 {
            bail!(invalid("description must be at most 500 characters."));
        }
        if !(1..=43_200).contains(&self.visibility_seconds) {
            bail!(invalid("visibility_seconds must be 1-43200 (12 hours)."));
        }
        if !(1..=1000).contains(&self.max_receives) {
            bail!(invalid("max_receives must be 1-1000."));
        }
        if !(1..=24 * 365).contains(&self.retention_hours) {
            bail!(invalid("retention_hours must be 1-8760."));
        }
        if self.dead_letter.as_deref() == Some(self.name.as_str()) {
            bail!(invalid("A queue can't be its own dead-letter queue."));
        }
        Ok(())
    }
}

/// A message to send.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewQueueMessage {
    pub body: Value,
    /// Ready only after this many seconds (at most 15 minutes).
    #[serde(default)]
    pub delay_seconds: u64,
    /// A message with the same key sent in the last 5 minutes isn't sent again.
    #[serde(default)]
    pub dedupe_key: Option<String>,
}

/// A message as stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    body: Value,
    sent_at: i64,
    sent_by: String,
    /// Ready from this time (epoch ms): after a delay, or when a lease runs out.
    ready_at: i64,
    /// The current lease's nonce (none: not leased).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lease: Option<String>,
    #[serde(default)]
    receives: u64,
    /// Queues it went through on its way to a dead-letter queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dead_from: Option<String>,
}

/// A received message.
#[derive(Debug, Clone, Serialize)]
pub struct Received {
    pub id: String,
    pub receipt: String,
    pub body: Value,
    pub receives: u64,
    pub sent_at: i64,
}

/// One queue's in-memory state on the Overseer.
#[derive(Default)]
struct QueueState {
    loaded: bool,
    /// (ready_at, id) for every message.
    ready: BTreeSet<(i64, Ulid)>,
    /// When each message is next ready (to update `ready`).
    at: HashMap<Ulid, i64>,
    /// Messages moved in from another queue (dead letters), until when they may still be
    /// committing: one not found yet is tried again rather than forgotten.
    arriving: HashMap<Ulid, i64>,
}

/// Queue state on this hex.
#[derive(Default)]
pub struct Queues {
    states: Mutex<HashMap<String, Arc<tokio::sync::Mutex<QueueState>>>>,
    /// Wakes long-polling receivers when messages are sent or released.
    notify: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    /// Messages moved into a queue (its dead letters) since its index was loaded:
    /// added to the index by its next operation, so a move never waits for the
    /// dead-letter queue's own lock (two queues can be each other's target).
    arrived: Mutex<HashMap<String, Vec<(i64, Ulid)>>>,
}

impl Queues {
    fn state(&self, name: &str) -> Arc<tokio::sync::Mutex<QueueState>> {
        self.states.lock().unwrap().entry(name.to_string()).or_default().clone()
    }

    fn notifier(&self, name: &str) -> Arc<tokio::sync::Notify> {
        self.notify.lock().unwrap().entry(name.to_string()).or_default().clone()
    }

    /// Forget every queue's state (a new Overseer reloads it from the documents).
    pub fn reset(&self) {
        self.states.lock().unwrap().clear();
        self.arrived.lock().unwrap().clear();
    }
}

fn parse_receipt(receipt: &str) -> Option<(Ulid, &str)> {
    let (id, nonce) = receipt.split_once('.')?;
    Some((Ulid::from_string(id).ok()?, nonce))
}

impl HexDBEngine {
    /// Every queue.
    pub async fn list_queues(&self) -> Result<Vec<QueueConfig>> {
        if !self.tessellation_exists(QUEUES_TESSELLATION) {
            return Ok(Vec::new());
        }
        let page = self.list_documents(QUEUES_TESSELLATION, None, usize::MAX).await?;
        let mut list: Vec<QueueConfig> = page.documents.iter().filter_map(|d| serde_json::from_value(d.data_json()).ok()).collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(list)
    }

    pub async fn get_queue(&self, name: &str) -> Result<Option<QueueConfig>> {
        let Some(doc) = self.get_system_document(QUEUES_TESSELLATION, config_id(name)).await? else { return Ok(None) };
        Ok(serde_json::from_value(doc.data_json()).ok())
    }

    async fn queue_or_404(&self, name: &str) -> Result<QueueConfig> {
        self.get_queue(name).await?.ok_or_else(|| EngineError::NotFound(format!("Queue '{}' not found.", name)).into())
    }

    /// Create or replace a queue's configuration.
    pub async fn save_queue(&self, mut config: QueueConfig, by: &str, create: bool) -> Result<QueueConfig> {
        self.ensure_writable()?;
        config.validate()?;
        if let Some(dead) = &config.dead_letter {
            self.queue_or_404(dead).await.map_err(|_| invalid(format!("The dead-letter queue '{}' doesn't exist; create it first.", dead)))?;
            // Follow the chain of dead-letter queues: it must not lead back here.
            let (mut next, mut seen) = (Some(dead.clone()), std::collections::HashSet::new());
            while let Some(name) = next {
                if name == config.name {
                    bail!(invalid(format!("Dead-letter queues can't form a cycle: '{}' leads back to '{}'.", dead, config.name)));
                }
                if !seen.insert(name.clone()) {
                    break;
                }
                next = self.get_queue(&name).await?.and_then(|q| q.dead_letter);
            }
        }
        let existing = self.get_queue(&config.name).await?;
        match (&existing, create) {
            (Some(_), true) => bail!(EngineError::Conflict(format!("A queue named '{}' already exists.", config.name))),
            (None, false) => bail!(EngineError::NotFound(format!("Queue '{}' not found.", config.name))),
            (Some(old), false) => {
                config.created = old.created;
                config.created_by = old.created_by.clone();
            }
            (None, true) => {
                config.created = now_ms();
                config.created_by = by.to_string();
            }
        }
        self.ensure_system_tessellation(&messages_tessellation(&config.name))?;
        self.put_system_document(QUEUES_TESSELLATION, config_id(&config.name), serde_json::to_value(&config)?, None).await?;
        Ok(config)
    }

    /// Delete a queue with its messages.
    pub async fn delete_queue(&self, name: &str) -> Result<bool> {
        self.ensure_writable()?;
        if self.get_queue(name).await?.is_none() {
            return Ok(false);
        }
        if let Some(user) = self.list_queues().await?.into_iter().find(|q| q.dead_letter.as_deref() == Some(name)) {
            bail!(EngineError::Conflict(format!("Queue '{}' sends its dead letters here; change it first.", user.name)));
        }
        self.delete_system_document(QUEUES_TESSELLATION, config_id(name)).await?;
        self.delete_tessellation_unchecked(&messages_tessellation(name)).await?;
        self.queues.states.lock().unwrap().remove(name);
        self.queues.arrived.lock().unwrap().remove(name);
        // Its de-duplication keys, so a new queue of this name starts afresh.
        if self.tessellation_exists(DEDUPE_TESSELLATION) {
            let page = self.list_documents(DEDUPE_TESSELLATION, None, usize::MAX).await?;
            let ids: Vec<(String, Ulid)> = page.documents.iter().filter(|d| d.data_json()["queue"] == name).map(|d| (DEDUPE_TESSELLATION.to_string(), d.id)).collect();
            if !ids.is_empty() {
                self.commit_system(Vec::new(), ids).await?;
            }
        }
        Ok(true)
    }

    /// Load a queue's index from its documents (once per Overseer term).
    async fn load_queue(&self, name: &str, state: &mut QueueState) -> Result<()> {
        if !state.loaded {
            self.reload_queue(name, state).await?;
            // Dead letters still committing survive a reload: they're looked for again.
            let now = now_ms();
            state.arriving.retain(|_, until| *until > now);
            let pending: Vec<Ulid> = state.arriving.keys().filter(|id| !state.at.contains_key(*id)).copied().collect();
            for id in pending {
                Self::set_ready(state, id, Some(now));
            }
        }
        // Taken after reading the documents, so a move whose commit the read missed is still added.
        let arrived = self.queues.arrived.lock().unwrap().remove(name).unwrap_or_default();
        for (at, id) in arrived {
            if !state.at.contains_key(&id) {
                Self::set_ready(state, id, Some(at));
            }
            state.arriving.insert(id, now_ms() + ARRIVAL_GRACE_MS);
        }
        Ok(())
    }

    async fn reload_queue(&self, name: &str, state: &mut QueueState) -> Result<()> {
        state.ready.clear();
        state.at.clear();
        let tess = messages_tessellation(name);
        if self.tessellation_exists(&tess) {
            let mut after = None;
            loop {
                let page = self.list_documents(&tess, after, 1000).await?;
                for doc in &page.documents {
                    if let Ok(stored) = serde_json::from_value::<Stored>(doc.data_json()) {
                        state.ready.insert((stored.ready_at, doc.id));
                        state.at.insert(doc.id, stored.ready_at);
                    }
                }
                match page.next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
        }
        state.loaded = true;
        Ok(())
    }

    fn set_ready(state: &mut QueueState, id: Ulid, ready_at: Option<i64>) {
        if let Some(old) = state.at.remove(&id) {
            state.ready.remove(&(old, id));
        }
        if let Some(at) = ready_at {
            state.ready.insert((at, id));
            state.at.insert(id, at);
        }
    }

    async fn put_message(&self, tess: &str, id: Ulid, stored: &Stored, retention_hours: u64) -> Result<()> {
        let ttl = stored.sent_at + retention_hours as i64 * 3_600_000;
        self.put_system_document(tess, id, serde_json::to_value(stored)?, Some(ttl)).await
    }

    /// Send messages; returns their IDs (none for de-duplicated ones).
    pub async fn send_messages(&self, name: &str, messages: Vec<NewQueueMessage>, by: &str) -> Result<Vec<Option<String>>> {
        self.ensure_writable()?;
        let config = self.queue_or_404(name).await?;
        if messages.is_empty() || messages.len() > MAX_BATCH {
            bail!(invalid(format!("Send 1-{} messages at a time.", MAX_BATCH)));
        }
        for m in &messages {
            if m.delay_seconds > 900 {
                bail!(invalid("delay_seconds is at most 900 (15 minutes)."));
            }
            if serde_json::to_vec(&m.body)?.len() > MAX_BODY_BYTES {
                bail!(invalid(format!("A message body is at most {} KB.", MAX_BODY_BYTES / 1024)));
            }
            if m.dedupe_key.as_ref().is_some_and(|k| k.is_empty() || k.len() > 128) {
                bail!(invalid("dedupe_key is 1-128 characters."));
            }
        }
        let tess = messages_tessellation(name);
        self.ensure_system_tessellation(&tess)?;
        let state = self.queues.state(name);
        let mut state = state.lock().await;
        self.load_queue(name, &mut state).await?;
        let now = now_ms();
        let mut ids = Vec::with_capacity(messages.len());
        // Unloaded until the index is updated below (see the module comment).
        state.loaded = false;
        for m in messages {
            let id = self.stream_ids.lock().unwrap().generate().unwrap_or_else(|_| Ulid::generate());
            let stored = Stored { body: m.body, sent_at: now, sent_by: by.to_string(), ready_at: now + m.delay_seconds as i64 * 1000, lease: None, receives: 0, dead_from: None };
            let ttl = stored.sent_at + config.retention_hours as i64 * 3_600_000;
            let mut docs = vec![crate::document::Document { id, tessellation: tess.clone(), data: crate::document::infer_fields_from_json(&serde_json::to_value(&stored)?), ttl: Some(ttl) }];
            if let Some(key) = &m.dedupe_key {
                if self.seen_dedupe_key(name, key, now).await? {
                    ids.push(None);
                    continue;
                }
                // Stored with the message, so the key counts only if the message does.
                let data = json!({ "queue": name, "key": key, "message": id.to_string(), "at": now });
                docs.push(crate::document::Document {
                    id: dedupe_id(name, key),
                    tessellation: DEDUPE_TESSELLATION.to_string(),
                    data: crate::document::infer_fields_from_json(&data),
                    ttl: Some(now + DEDUPE_MS),
                });
            }
            self.commit_system(docs, Vec::new()).await?;
            Self::set_ready(&mut state, id, Some(stored.ready_at));
            ids.push(Some(id.to_string()));
        }
        state.loaded = true;
        drop(state);
        self.queues.notifier(name).notify_waiters();
        Ok(ids)
    }

    /// Lease up to `max` ready messages for `visibility` (the queue's own
    /// timeout by default), waiting up to `wait` for some to arrive.
    pub async fn receive_messages(&self, name: &str, max: usize, visibility: Option<u64>, wait: Duration) -> Result<Vec<Received>> {
        self.ensure_writable()?;
        let config = self.queue_or_404(name).await?;
        let max = max.clamp(1, MAX_BATCH);
        let visibility_ms = visibility.unwrap_or(config.visibility_seconds).clamp(1, 43_200) as i64 * 1000;
        let deadline = tokio::time::Instant::now() + wait.min(Duration::from_secs(MAX_WAIT_SECONDS));
        loop {
            let notified = self.queues.notifier(name);
            let notified = notified.notified();
            let got = self.lease_ready(name, &config, max, visibility_ms).await?;
            if !got.is_empty() || tokio::time::Instant::now() >= deadline {
                return Ok(got);
            }
            // Wake on a send or release, or when the next message's lease or delay runs out.
            let next = self.next_ready_in(name).await.unwrap_or(Duration::from_secs(1)).min(Duration::from_secs(1));
            let _ = tokio::time::timeout_at(deadline.min(tokio::time::Instant::now() + next), notified).await;
        }
    }

    /// True if `key` was sent to this queue in the last 5 minutes.
    async fn seen_dedupe_key(&self, name: &str, key: &str, now: i64) -> Result<bool> {
        if !self.tessellation_exists(DEDUPE_TESSELLATION) {
            return Ok(false);
        }
        let Some(doc) = self.get_system_document(DEDUPE_TESSELLATION, dedupe_id(name, key)).await? else { return Ok(false) };
        let data = doc.data_json();
        Ok(data["queue"] == name && data["key"] == key && data["at"].as_i64().is_some_and(|at| now - at < DEDUPE_MS))
    }

    async fn next_ready_in(&self, name: &str) -> Option<Duration> {
        let state = self.queues.state(name);
        let state = state.lock().await;
        let (at, _) = state.ready.iter().next()?;
        Some(Duration::from_millis((at - now_ms()).max(0) as u64))
    }

    async fn lease_ready(&self, name: &str, config: &QueueConfig, max: usize, visibility_ms: i64) -> Result<Vec<Received>> {
        let tess = messages_tessellation(name);
        let state = self.queues.state(name);
        let mut state = state.lock().await;
        self.load_queue(name, &mut state).await?;
        let now = now_ms();
        // A bounded look at the front of the index, not every due message: entries this call finds
        // gone, delayed or dead are moved out of the way, so the next call goes further.
        let due: Vec<Ulid> = state.ready.iter().take_while(|(at, _)| *at <= now).take(max.saturating_mul(4).max(64)).map(|(_, id)| *id).collect();
        let mut out = Vec::new();
        // Unloaded until the index is updated below (see the module comment).
        state.loaded = false;
        for id in due {
            if out.len() >= max {
                break;
            }
            let Some(doc) = self.get_system_document(&tess, id).await? else {
                // A dead letter whose move hasn't committed yet: look again shortly.
                if state.arriving.get(&id).is_some_and(|until| now < *until) {
                    Self::set_ready(&mut state, id, Some(now + 500));
                    continue;
                }
                // Expired by retention or deleted: forget it.
                state.arriving.remove(&id);
                Self::set_ready(&mut state, id, None);
                continue;
            };
            state.arriving.remove(&id);
            let Ok(mut stored) = serde_json::from_value::<Stored>(doc.data_json()) else {
                Self::set_ready(&mut state, id, None);
                continue;
            };
            // The stored message decides: leased or delayed after all, it waits.
            if stored.ready_at > now {
                Self::set_ready(&mut state, id, Some(stored.ready_at));
                continue;
            }
            // Received too often without an ack: off to the dead-letter queue.
            if stored.receives >= config.max_receives {
                self.dead_letter(config, id, stored).await?;
                Self::set_ready(&mut state, id, None);
                continue;
            }
            let nonce = hex::encode(crate::crypt::random_bytes(12));
            stored.lease = Some(nonce.clone());
            stored.receives += 1;
            stored.ready_at = now + visibility_ms;
            self.put_message(&tess, id, &stored, config.retention_hours).await?;
            Self::set_ready(&mut state, id, Some(stored.ready_at));
            out.push(Received { id: id.to_string(), receipt: format!("{}.{}", id, nonce), body: stored.body, receives: stored.receives, sent_at: stored.sent_at });
        }
        state.loaded = true;
        Ok(out)
    }

    /// Move a message to the queue's dead-letter queue (or drop it), in one
    /// commit: it's in exactly one of the two queues whatever happens.
    async fn dead_letter(&self, config: &QueueConfig, id: Ulid, mut stored: Stored) -> Result<()> {
        let source = (messages_tessellation(&config.name), id);
        let target = match &config.dead_letter {
            Some(dead) => self.get_queue(dead).await?,
            None => None,
        };
        let Some(target) = target else {
            match &config.dead_letter {
                None => tracing::warn!("⚠️ Queue '{}' dropped message {} after {} receives (no dead-letter queue).", config.name, id, stored.receives),
                Some(dead) => tracing::warn!("⚠️ Queue '{}' dropped message {}: its dead-letter queue '{}' is gone.", config.name, id, dead),
            }
            return self.commit_system(Vec::new(), vec![source]).await;
        };
        let now = now_ms();
        stored.dead_from = Some(config.name.clone());
        stored.lease = None;
        stored.receives = 0;
        stored.ready_at = now;
        stored.sent_at = now;
        let moved = crate::document::Document {
            id,
            tessellation: messages_tessellation(&target.name),
            data: crate::document::infer_fields_from_json(&serde_json::to_value(&stored)?),
            ttl: Some(now + target.retention_hours as i64 * 3_600_000),
        };
        // Into the dead-letter queue's index at its next operation (a stale entry, if this
        // commit doesn't happen, is dropped when its document isn't found).
        self.queues.arrived.lock().unwrap().entry(target.name.clone()).or_default().push((now, id));
        self.commit_system(vec![moved], vec![source]).await?;
        self.queues.notifier(&target.name).notify_waiters();
        Ok(())
    }

    /// The message a receipt names, if the receipt's lease is still current.
    async fn leased(&self, name: &str, receipt: &str) -> Result<Option<(Ulid, Stored)>> {
        let Some((id, nonce)) = parse_receipt(receipt) else { return Ok(None) };
        let Some(doc) = self.get_system_document(&messages_tessellation(name), id).await? else { return Ok(None) };
        let Ok(stored) = serde_json::from_value::<Stored>(doc.data_json()) else { return Ok(None) };
        let current = stored.lease.as_deref().is_some_and(|l| crate::crypt::constant_time_eq(l.as_bytes(), nonce.as_bytes())) && stored.ready_at > now_ms();
        Ok(current.then_some((id, stored)))
    }

    /// Acknowledge (delete) messages; returns how many receipts were current.
    pub async fn ack_messages(&self, name: &str, receipts: &[String]) -> Result<usize> {
        self.ensure_writable()?;
        self.queue_or_404(name).await?;
        let state = self.queues.state(name);
        let mut state = state.lock().await;
        self.load_queue(name, &mut state).await?;
        let mut done = 0;
        state.loaded = false;
        for receipt in receipts.iter().take(MAX_BATCH) {
            if let Some((id, _)) = self.leased(name, receipt).await? {
                self.delete_system_document(&messages_tessellation(name), id).await?;
                Self::set_ready(&mut state, id, None);
                done += 1;
            }
        }
        state.loaded = true;
        Ok(done)
    }

    /// Give messages back: ready again now, or after `delay_seconds`; or, with
    /// `extend`, keep the lease for `seconds` more. Returns how many receipts were current.
    pub async fn release_messages(&self, name: &str, receipts: &[String], seconds: u64, extend: bool) -> Result<usize> {
        self.ensure_writable()?;
        let config = self.queue_or_404(name).await?;
        if seconds > 43_200 {
            bail!(invalid("At most 43200 seconds (12 hours)."));
        }
        let tess = messages_tessellation(name);
        let state = self.queues.state(name);
        let mut state = state.lock().await;
        self.load_queue(name, &mut state).await?;
        let mut done = 0;
        state.loaded = false;
        for receipt in receipts.iter().take(MAX_BATCH) {
            if let Some((id, mut stored)) = self.leased(name, receipt).await? {
                stored.ready_at = now_ms() + seconds as i64 * 1000;
                if !extend {
                    stored.lease = None;
                }
                self.put_message(&tess, id, &stored, config.retention_hours).await?;
                Self::set_ready(&mut state, id, Some(stored.ready_at));
                done += 1;
            }
        }
        state.loaded = true;
        drop(state);
        if !extend {
            self.queues.notifier(name).notify_waiters();
        }
        Ok(done)
    }

    /// How many messages are ready, leased (in flight) and delayed.
    pub async fn queue_stats(&self, name: &str) -> Result<Value> {
        let config = self.queue_or_404(name).await?;
        let tess = messages_tessellation(name);
        let (mut ready, mut leased, mut delayed, mut oldest) = (0u64, 0u64, 0u64, None::<i64>);
        if self.tessellation_exists(&tess) {
            let now = now_ms();
            let mut after = None;
            loop {
                let page = self.list_documents(&tess, after, 1000).await?;
                for doc in &page.documents {
                    let Ok(stored) = serde_json::from_value::<Stored>(doc.data_json()) else { continue };
                    if stored.ready_at <= now {
                        ready += 1;
                    } else if stored.lease.is_some() {
                        leased += 1;
                    } else {
                        delayed += 1;
                    }
                    oldest = Some(oldest.map_or(stored.sent_at, |o| o.min(stored.sent_at)));
                }
                match page.next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
        }
        Ok(json!({ "queue": config, "ready": ready, "in_flight": leased, "delayed": delayed, "oldest_sent_at": oldest }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipts_parse_and_configs_validate() {
        let id = Ulid::generate();
        assert_eq!(parse_receipt(&format!("{}.abc", id)), Some((id, "abc")));
        assert!(parse_receipt("nonsense").is_none());
        let mut config = QueueConfig { name: "jobs".into(), description: String::new(), visibility_seconds: 30, max_receives: 5, dead_letter: None, retention_hours: 96, created: 0, created_by: String::new() };
        assert!(config.validate().is_ok());
        config.dead_letter = Some("jobs".into());
        assert!(config.validate().is_err(), "not its own dead-letter queue");
        config.dead_letter = None;
        config.visibility_seconds = 0;
        assert!(config.validate().is_err());
    }

    #[tokio::test]
    async fn messages_go_to_one_consumer_return_when_unacked_and_end_up_dead() {
        let (engine, dir) = crate::engine::test_engine().await;
        let base = |name: &str, dead: Option<&str>| QueueConfig {
            name: name.into(),
            description: String::new(),
            visibility_seconds: 1,
            max_receives: 2,
            dead_letter: dead.map(String::from),
            retention_hours: 1,
            created: 0,
            created_by: String::new(),
        };
        engine.save_queue(base("dead", None), "t", true).await.unwrap();
        engine.save_queue(base("jobs", Some("dead")), "t", true).await.unwrap();
        let send = |n: i64| NewQueueMessage { body: json!({ "n": n }), delay_seconds: 0, dedupe_key: None };
        engine.send_messages("jobs", vec![send(1), send(2)], "t").await.unwrap();

        // Each message goes to one consumer.
        let first = engine.receive_messages("jobs", 1, None, Duration::ZERO).await.unwrap();
        let second = engine.receive_messages("jobs", 10, None, Duration::ZERO).await.unwrap();
        assert_eq!((first.len(), second.len()), (1, 1));
        assert_ne!(first[0].id, second[0].id);
        assert!(engine.receive_messages("jobs", 10, None, Duration::ZERO).await.unwrap().is_empty(), "both leased");

        // An ack deletes; a stale receipt does nothing.
        assert_eq!(engine.ack_messages("jobs", &[first[0].receipt.clone()]).await.unwrap(), 1);
        assert_eq!(engine.ack_messages("jobs", &[first[0].receipt.clone()]).await.unwrap(), 0);

        // The other lease runs out: received again (2 of 2), then dead-lettered.
        let again = engine.receive_messages("jobs", 10, None, Duration::from_secs(3)).await.unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].receives, 2);
        assert_eq!(engine.ack_messages("jobs", &[second[0].receipt.clone()]).await.unwrap(), 0, "the old lease's receipt is stale");
        let none = engine.receive_messages("jobs", 10, None, Duration::from_secs(3)).await.unwrap();
        assert!(none.is_empty(), "moved to the dead-letter queue: {:?}", none);
        let dead = engine.receive_messages("dead", 10, None, Duration::ZERO).await.unwrap();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].body, json!({ "n": 2 }));

        // De-duplication and nack.
        let dup = |key: &str| NewQueueMessage { body: json!("x"), delay_seconds: 0, dedupe_key: Some(key.into()) };
        let ids = engine.send_messages("jobs", vec![dup("k"), dup("k")], "t").await.unwrap();
        assert!(ids[0].is_some() && ids[1].is_none());
        let got = engine.receive_messages("jobs", 10, None, Duration::ZERO).await.unwrap();
        assert_eq!(engine.release_messages("jobs", &[got[0].receipt.clone()], 0, false).await.unwrap(), 1);
        assert_eq!(engine.receive_messages("jobs", 10, None, Duration::ZERO).await.unwrap().len(), 1, "ready again at once");
        engine.shutdown().await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    // Fault injection from the 2.5.5 review: the WAL is paused, the request is
    // dropped while it waits, then the WAL goes on and a later write fences it.

    fn config(name: &str, dead: Option<&str>) -> QueueConfig {
        QueueConfig { name: name.into(), description: String::new(), visibility_seconds: 60, max_receives: 1, dead_letter: dead.map(String::from), retention_hours: 1, created: 0, created_by: String::new() }
    }

    fn message() -> NewQueueMessage {
        NewQueueMessage { body: json!("synthetic-job"), delay_seconds: 0, dedupe_key: None }
    }

    #[tokio::test]
    async fn review255_canceled_send_remains_deliverable() {
        let (e, dir) = crate::engine::test_engine().await;
        e.save_queue(config("jobs", None), "test", true).await.unwrap();
        e.receive_messages("jobs", 1, None, Duration::ZERO).await.unwrap();
        let release = e.pause_wal_for_review().await;
        let mut write = Box::pin(e.send_messages("jobs", vec![message()], "test"));
        assert!(futures::poll!(write.as_mut()).is_pending());
        assert_eq!(e.queue_stats("jobs").await.unwrap()["ready"], 1, "reached the committed message");
        drop(write);
        release.send(()).unwrap();
        e.insert_json("fence", json!({"x": 1}), None).await.unwrap();
        let got = e.receive_messages("jobs", 1, None, Duration::ZERO).await.unwrap();
        e.shutdown().await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(got.len(), 1, "Durable message was stranded outside the ready index after cancellation");
    }

    #[tokio::test]
    async fn review255_canceled_receive_keeps_lease_until_expiry() {
        let (e, dir) = crate::engine::test_engine().await;
        let mut cfg = config("jobs", None);
        cfg.max_receives = 5;
        e.save_queue(cfg, "test", true).await.unwrap();
        e.send_messages("jobs", vec![message()], "test").await.unwrap();
        let release = e.pause_wal_for_review().await;
        let mut recv = Box::pin(e.receive_messages("jobs", 1, None, Duration::ZERO));
        assert!(futures::poll!(recv.as_mut()).is_pending());
        assert_eq!(e.queue_stats("jobs").await.unwrap()["in_flight"], 1, "reached the committed lease");
        drop(recv);
        release.send(()).unwrap();
        e.insert_json("fence", json!({"x": 1}), None).await.unwrap();
        let got = e.receive_messages("jobs", 1, None, Duration::ZERO).await.unwrap();
        e.shutdown().await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert!(got.is_empty(), "A committed 60-second lease was immediately redelivered after cancellation");
    }

    #[tokio::test]
    async fn review255_dead_letter_cancellation_preserves_message() {
        let (e, dir) = crate::engine::test_engine().await;
        e.save_queue(config("dead", None), "test", true).await.unwrap();
        e.save_queue(config("jobs", Some("dead")), "test", true).await.unwrap();
        e.send_messages("jobs", vec![message()], "test").await.unwrap();
        let got = e.receive_messages("jobs", 1, None, Duration::ZERO).await.unwrap();
        e.release_messages("jobs", &[got[0].receipt.clone()], 0, false).await.unwrap();
        let release = e.pause_wal_for_review().await;
        let mut recv = Box::pin(e.receive_messages("jobs", 1, None, Duration::ZERO));
        assert!(futures::poll!(recv.as_mut()).is_pending());
        assert_eq!(e.queue_stats("jobs").await.unwrap()["ready"], 0, "reached source deletion");
        drop(recv);
        release.send(()).unwrap();
        e.insert_json("fence", json!({"x": 1}), None).await.unwrap();
        let a = e.queue_stats("jobs").await.unwrap();
        let b = e.queue_stats("dead").await.unwrap();
        // And the dead-letter queue hands it out.
        let dead = e.receive_messages("dead", 10, None, Duration::ZERO).await.unwrap();
        e.shutdown().await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(a["ready"].as_u64().unwrap() + b["ready"].as_u64().unwrap(), 1, "Dead-letter cancellation durably lost the message from both queues");
        assert_eq!(dead.len(), 1);
    }

    /// H256-02: a receive that runs between a dead letter's arrival and its commit must not
    /// forget it: once the move commits, it's delivered.
    #[tokio::test]
    async fn review256_dead_letter_arrival_cannot_be_consumed_before_commit() {
        let (e, dir) = crate::engine::test_engine().await;
        e.save_queue(config("dead", None), "test", true).await.unwrap();
        assert!(e.receive_messages("dead", 1, None, Duration::ZERO).await.unwrap().is_empty(), "index loaded");
        let id = Ulid::generate();
        let now = now_ms();
        // The arrival is published, but the moved document isn't committed yet.
        e.queues.arrived.lock().unwrap().entry("dead".into()).or_default().push((now, id));
        assert!(e.receive_messages("dead", 1, None, Duration::ZERO).await.unwrap().is_empty());
        // The move commits.
        let stored = Stored { body: json!("synthetic-job"), sent_at: now, sent_by: "t".into(), ready_at: now, lease: None, receives: 0, dead_from: Some("jobs".into()) };
        let doc = crate::document::Document { id, tessellation: messages_tessellation("dead"), data: crate::document::infer_fields_from_json(&serde_json::to_value(&stored).unwrap()), ttl: None };
        e.commit_system(vec![doc], Vec::new()).await.unwrap();
        let got = e.receive_messages("dead", 1, None, Duration::from_secs(3)).await.unwrap();
        e.shutdown().await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(got.len(), 1, "the dead letter was stranded");
    }

    #[tokio::test]
    async fn dead_letter_queues_cannot_form_a_cycle_and_keys_outlive_the_index() {
        let (e, dir) = crate::engine::test_engine().await;
        e.save_queue(config("a", None), "test", true).await.unwrap();
        e.save_queue(config("b", Some("a")), "test", true).await.unwrap();
        e.save_queue(config("c", Some("b")), "test", true).await.unwrap();
        let err = e.save_queue(config("a", Some("c")), "test", false).await.unwrap_err();
        assert!(err.to_string().contains("cycle"), "{}", err);

        // A de-duplication key is a document, not just memory: a new Overseer's empty index keeps it.
        let keyed = || NewQueueMessage { body: json!("x"), delay_seconds: 0, dedupe_key: Some("same".into()) };
        assert!(e.send_messages("a", vec![keyed()], "test").await.unwrap()[0].is_some());
        e.queues.reset();
        assert!(e.send_messages("a", vec![keyed()], "test").await.unwrap()[0].is_none());
        // Acknowledged or not, the key holds for its 5 minutes; another queue's key is separate.
        let got = e.receive_messages("a", 1, None, Duration::ZERO).await.unwrap();
        e.ack_messages("a", &[got[0].receipt.clone()]).await.unwrap();
        assert!(e.send_messages("a", vec![keyed()], "test").await.unwrap()[0].is_none());
        assert!(e.send_messages("b", vec![keyed()], "test").await.unwrap()[0].is_some());
        e.shutdown().await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
