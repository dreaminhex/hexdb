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
/// How long a de-duplication key is remembered.
const DEDUPE_MS: i64 = 5 * 60 * 1000;

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
    /// Recent de-duplication keys -> when they were sent.
    dedupe: HashMap<String, i64>,
}

/// Queue state on this hex.
#[derive(Default)]
pub struct Queues {
    states: Mutex<HashMap<String, Arc<tokio::sync::Mutex<QueueState>>>>,
    /// Wakes long-polling receivers when messages are sent or released.
    notify: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
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
        Ok(true)
    }

    /// Load a queue's index from its documents (once per Overseer term).
    async fn load_queue(&self, name: &str, state: &mut QueueState) -> Result<()> {
        if state.loaded {
            return Ok(());
        }
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
        state.dedupe.retain(|_, at| now - *at < DEDUPE_MS);
        let mut ids = Vec::with_capacity(messages.len());
        for m in messages {
            if let Some(key) = &m.dedupe_key {
                if state.dedupe.contains_key(key) {
                    ids.push(None);
                    continue;
                }
                state.dedupe.insert(key.clone(), now);
            }
            let id = self.stream_ids.lock().unwrap().generate().unwrap_or_else(|_| Ulid::new());
            let stored = Stored { body: m.body, sent_at: now, sent_by: by.to_string(), ready_at: now + m.delay_seconds as i64 * 1000, lease: None, receives: 0, dead_from: None };
            self.put_message(&tess, id, &stored, config.retention_hours).await?;
            Self::set_ready(&mut state, id, Some(stored.ready_at));
            ids.push(Some(id.to_string()));
        }
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
        let due: Vec<Ulid> = state.ready.iter().take_while(|(at, _)| *at <= now).map(|(_, id)| *id).collect();
        let mut out = Vec::new();
        for id in due {
            if out.len() >= max {
                break;
            }
            let Some(doc) = self.get_system_document(&tess, id).await? else {
                // Expired by retention or deleted: forget it.
                Self::set_ready(&mut state, id, None);
                continue;
            };
            let Ok(mut stored) = serde_json::from_value::<Stored>(doc.data_json()) else {
                Self::set_ready(&mut state, id, None);
                continue;
            };
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
        Ok(out)
    }

    /// Move a message to the queue's dead-letter queue (or drop it).
    async fn dead_letter(&self, config: &QueueConfig, id: Ulid, mut stored: Stored) -> Result<()> {
        self.delete_system_document(&messages_tessellation(&config.name), id).await?;
        let Some(dead) = &config.dead_letter else {
            tracing::warn!("⚠️ Queue '{}' dropped message {} after {} receives (no dead-letter queue).", config.name, id, stored.receives);
            return Ok(());
        };
        let Some(target) = self.get_queue(dead).await? else {
            tracing::warn!("⚠️ Queue '{}' dropped message {}: its dead-letter queue '{}' is gone.", config.name, id, dead);
            return Ok(());
        };
        let now = now_ms();
        stored.dead_from = Some(config.name.clone());
        stored.lease = None;
        stored.receives = 0;
        stored.ready_at = now;
        stored.sent_at = now;
        let tess = messages_tessellation(dead);
        self.ensure_system_tessellation(&tess)?;
        let state = self.queues.state(dead);
        let mut state = state.lock().await;
        self.load_queue(dead, &mut state).await?;
        self.put_message(&tess, id, &stored, target.retention_hours).await?;
        Self::set_ready(&mut state, id, Some(now));
        drop(state);
        self.queues.notifier(dead).notify_waiters();
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
        for receipt in receipts.iter().take(MAX_BATCH) {
            if let Some((id, _)) = self.leased(name, receipt).await? {
                self.delete_system_document(&messages_tessellation(name), id).await?;
                Self::set_ready(&mut state, id, None);
                done += 1;
            }
        }
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
        let id = Ulid::new();
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
}
