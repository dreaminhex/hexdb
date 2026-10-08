// HexDB Core Triggers: run a function when documents change
//
// A trigger names a tessellation, the events it reacts to (insert, update,
// delete), an optional filter on the document, and a function to run:
//
//   before   Runs inside the write, before it commits. Must be a script
//            function. It gets the event and can let the write through
//            (print `null` or `{}`), change the document
//            (`{"document": {...}}`), or refuse it (`{"reject": "why"}`,
//            which fails the write with 422 trigger_rejected). A failing
//            script refuses the write too. A before trigger may run more than
//            once for one write (a write that conflicts with another is
//            planned again), so it must not have side effects.
//
//   after    Runs once the write is committed, from the change feed, on the
//            Overseer. Any kind of function: a transaction that writes an
//            audit row, a script that calls a webhook, ... Its position in
//            the feed is saved (`_trigger_cursors`), so after a restart it
//            continues where it stopped; a failure is recorded and the trigger
//            moves on to the next change.
//
// What the function gets: `event`, `tessellation`, `id`, `document` (the new
// version; none for delete), `previous` (the stored version, for update and
// delete; before triggers only) and `user` (who wrote; before triggers
// only). Scripts read it as `trigger` on stdin; other functions receive the
// values their declared parameters name.
//
// Triggers run as the administrator who created them (their own
// restrictions don't apply). Writes made while a trigger runs (directly, or
// by its script through the API) don't fire triggers, so triggers can't
// cascade or loop. Internal writes (schema migrations, replication) don't
// fire triggers either. Definitions live in `_triggers`.

use crate::{
    auth::{Credential, Principal},
    changes::Change,
    document::Document,
    engine::{EngineError, HexDBEngine},
    filter::Filter,
};
use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;
use tracing::{debug, info, warn};
use ulid::Ulid;

pub const TRIGGERS_TESSELLATION: &str = "_triggers";
pub const TRIGGER_CURSORS_TESSELLATION: &str = "_trigger_cursors";
const EVENTS: [&str; 3] = ["insert", "update", "delete"];

/// When a trigger runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Timing {
    Before,
    #[default]
    After,
}

/// A trigger definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    /// Taken from the URL on PUT.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub tessellation: String,
    /// insert, update, delete (default: all three).
    #[serde(default = "all_events")]
    pub events: Vec<String>,
    #[serde(default)]
    pub timing: Timing,
    /// The function to run.
    pub function: String,
    /// Only documents matching this filter (the new version; the stored one for deletes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Value>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// The user it runs as (whoever created or last changed it).
    #[serde(default)]
    pub run_as: String,
    #[serde(default)]
    pub run_as_login: String,
    #[serde(default)]
    pub updated: i64,
}

fn all_events() -> Vec<String> {
    EVENTS.iter().map(|e| e.to_string()).collect()
}

fn yes() -> bool {
    true
}

/// How a trigger has been doing (kept in memory on each hex).
#[derive(Debug, Clone, Default, Serialize)]
pub struct TriggerStatus {
    pub runs: u64,
    pub failures: u64,
    pub rejected: u64,
    pub last_run: Option<i64>,
    pub last_error: Option<String>,
    pub last_ms: u64,
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    EngineError::Invalid(message.into()).into()
}

fn doc_id(name: &str) -> Ulid {
    let hash = blake3::derive_key("HexDB 2026 trigger id v1", name.as_bytes());
    Ulid::from(u128::from_be_bytes(hash[..16].try_into().unwrap()))
}

impl Trigger {
    fn validate(&self) -> Result<()> {
        if self.name.is_empty() || self.name.len() > 64 || !self.name.chars().all(|c| c.is_ascii_alphanumeric() || "_-".contains(c)) {
            bail!(invalid("Trigger names are 1-64 letters, digits, '_' or '-'."));
        }
        crate::catalog::validate_tessellation_name(&self.tessellation).map_err(|e| invalid(format!("tessellation: {}", e)))?;
        if self.events.is_empty() || self.events.iter().any(|e| !EVENTS.contains(&e.as_str())) {
            bail!(invalid("events: one or more of insert, update, delete."));
        }
        if let Some(filter) = &self.filter {
            Filter::parse(filter).map_err(|e| invalid(format!("filter: {:#}", e)))?;
        }
        if self.description.len() > 500 {
            bail!(invalid("description must be at most 500 characters."));
        }
        Ok(())
    }

    fn applies(&self, tessellation: &str, event: &str, doc: Option<&Document>) -> bool {
        if !self.enabled || self.tessellation != tessellation || !self.events.iter().any(|e| e == event) {
            return false;
        }
        match (&self.filter, doc) {
            (None, _) => true,
            (Some(f), Some(doc)) => Filter::parse(f).is_ok_and(|f| f.matches(doc)),
            (Some(_), None) => false,
        }
    }
}

/// The values a trigger's function receives.
fn payload(trigger: &Trigger, event: &str, tessellation: &str, id: Ulid, document: Option<&Document>, previous: Option<&Document>, user: Option<&str>) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("trigger".into(), json!(trigger.name));
    map.insert("event".into(), json!(event));
    map.insert("tessellation".into(), json!(tessellation));
    map.insert("id".into(), json!(id.to_string()));
    map.insert("document".into(), document.map(Document::to_api_json).unwrap_or(Value::Null));
    map.insert("previous".into(), previous.map(Document::to_api_json).unwrap_or(Value::Null));
    map.insert("user".into(), user.map(|u| json!(u)).unwrap_or(Value::Null));
    map
}

impl HexDBEngine {
    pub async fn list_triggers(&self) -> Result<Vec<Trigger>> {
        if !self.tessellation_exists(TRIGGERS_TESSELLATION) {
            return Ok(Vec::new());
        }
        let page = self.list_documents(TRIGGERS_TESSELLATION, None, usize::MAX).await?;
        let mut list: Vec<Trigger> = page.documents.iter().filter_map(|d| serde_json::from_value(d.data_json()).ok()).collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(list)
    }

    pub async fn get_trigger(&self, name: &str) -> Result<Option<Trigger>> {
        let Some(doc) = self.get_system_document(TRIGGERS_TESSELLATION, doc_id(name)).await? else { return Ok(None) };
        Ok(serde_json::from_value(doc.data_json()).ok())
    }

    /// Create (`create`) or replace a trigger, to run as `by`.
    pub async fn save_trigger(&self, mut trigger: Trigger, by: &Principal, create: bool) -> Result<Trigger> {
        trigger.validate()?;
        if self.is_system_tessellation(&trigger.tessellation) {
            bail!(invalid("Triggers can't be put on system tessellations."));
        }
        let function = self
            .get_function(&trigger.function)
            .await?
            .ok_or_else(|| invalid(format!("Function '{}' doesn't exist.", trigger.function)))?;
        if trigger.timing == Timing::Before && function.kind != "script" {
            bail!(invalid("A before trigger runs a script function (it decides whether and how the write goes ahead)."));
        }
        let exists = self.get_trigger(&trigger.name).await?.is_some();
        if create && exists {
            bail!(EngineError::Conflict(format!("A trigger named '{}' already exists.", trigger.name)));
        }
        if !create && !exists {
            bail!(EngineError::NotFound(format!("Trigger '{}' not found.", trigger.name)));
        }
        trigger.run_as = by.user_id.clone();
        trigger.run_as_login = by.login.clone();
        trigger.updated = Utc::now().timestamp_millis();
        self.put_system_document(TRIGGERS_TESSELLATION, doc_id(&trigger.name), serde_json::to_value(&trigger)?, None).await?;
        Ok(trigger)
    }

    pub async fn delete_trigger(&self, name: &str) -> Result<bool> {
        if self.get_trigger(name).await?.is_none() {
            return Ok(false);
        }
        self.delete_system_document(TRIGGERS_TESSELLATION, doc_id(name)).await?;
        self.trigger_status.lock().unwrap().remove(name);
        Ok(true)
    }

    /// A trigger's run status on this hex.
    pub fn trigger_status(&self, name: &str) -> TriggerStatus {
        self.trigger_status.lock().unwrap().get(name).cloned().unwrap_or_default()
    }

    /// The enabled triggers on a tessellation (cached until `_triggers` changes).
    async fn triggers_on(&self, tess: &str, timing: Timing) -> Vec<Trigger> {
        if !self.tessellation_exists(TRIGGERS_TESSELLATION) {
            return Vec::new();
        }
        let generation = self.tessellation_generation(TRIGGERS_TESSELLATION);
        let cached = self.trigger_cache.lock().unwrap().as_ref().filter(|(g, _)| *g == generation).map(|(_, list)| list.clone());
        let all = match cached {
            Some(list) => list,
            None => {
                let list = Arc::new(self.list_triggers().await.unwrap_or_default());
                *self.trigger_cache.lock().unwrap() = Some((generation, list.clone()));
                list
            }
        };
        all.iter().filter(|t| t.enabled && t.timing == timing && t.tessellation == tess).cloned().collect()
    }

    /// The principal a trigger runs as, with its restrictions.
    async fn trigger_principal(&self, trigger: &Trigger) -> Result<Principal> {
        let (_, user) = crate::users::find_by_id(self, &trigger.run_as).await?.ok_or_else(|| anyhow!("its owner '{}' no longer exists", trigger.run_as_login))?;
        if user.is_locked {
            bail!("its owner '{}' is locked", user.login);
        }
        let definitions = crate::users::role_definitions(self).await?;
        Ok(Principal::new(trigger.run_as.clone(), user.login, user.email_address, user.roles, Credential::ApiKey { key_id: format!("trigger:{}", trigger.name) }, &definitions)
            .with_attributes(user.attributes))
    }

    /// Run a trigger's function with the event (as the trigger's owner, and
    /// marked as the trigger's work so its writes don't fire triggers).
    ///
    /// Returns a boxed future: a trigger's function may write, and writes run
    /// triggers, so the future type is recursive.
    fn run_trigger<'a>(&'a self, trigger: &'a Trigger, event: Map<String, Value>) -> futures::future::BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
        let started = std::time::Instant::now();
        let result = async {
            let def = self.get_function(&trigger.function).await?.ok_or_else(|| anyhow!("function '{}' no longer exists", trigger.function))?;
            let principal = Arc::new(self.trigger_principal(trigger).await?);
            // Non-script functions get the event values their parameters name.
            let args: Map<String, Value> = event.iter().filter(|(k, _)| def.params.iter().any(|p| &p.name == *k)).map(|(k, v)| (k.clone(), v.clone())).collect();
            let run = self.run_function_with(&def, &principal, &args, Some(("trigger", Value::Object(event))));
            crate::access::as_trigger(trigger.name.clone(), crate::access::as_caller(principal.clone(), run)).await
        }
        .await;
        let mut statuses = self.trigger_status.lock().unwrap();
        let status = statuses.entry(trigger.name.clone()).or_default();
        status.runs += 1;
        status.last_run = Some(Utc::now().timestamp_millis());
        status.last_ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(_) => status.last_error = None,
            Err(e) => {
                status.failures += 1;
                status.last_error = Some(format!("{:#}", e));
            }
        }
        drop(statuses);
        result
        })
    }

    /// Run the before triggers for a batch of planned writes (see the module
    /// comment). Applies their changes to the documents, or refuses the write.
    /// Writes with no caller (internal work) or made by a trigger skip this.
    pub(crate) async fn run_before_triggers(&self, items: &mut [crate::engine::writes::BatchItem]) -> Result<()> {
        if crate::access::current_trigger().is_some() {
            return Ok(());
        }
        let Some(caller) = crate::access::caller() else { return Ok(()) };
        for item in items.iter_mut() {
            let tess = item.key.tessellation.clone();
            if tess.starts_with('_') || self.is_system_tessellation(&tess) {
                continue;
            }
            let triggers = self.triggers_on(&tess, Timing::Before).await;
            if triggers.is_empty() {
                continue;
            }
            let (previous, _) = self.read_latest(&item.key).await?;
            let new = match &item.op {
                crate::wal::WalOp::Put(doc) => Some(doc.clone()),
                _ => None,
            };
            let event = match (&new, &previous) {
                (Some(_), None) => "insert",
                (Some(_), Some(_)) => "update",
                (None, _) => "delete",
            };
            for trigger in &triggers {
                let current = match &item.op {
                    crate::wal::WalOp::Put(doc) => Some(doc.clone()),
                    _ => None,
                };
                let subject = if event == "delete" { previous.as_ref() } else { current.as_ref() };
                if !trigger.applies(&tess, event, subject) {
                    continue;
                }
                let input = payload(trigger, event, &tess, item.key.id, current.as_ref(), previous.as_ref(), Some(&caller.login));
                let output = self.run_trigger(trigger, input).await.map_err(|e| -> anyhow::Error {
                    EngineError::TriggerRejected(format!("Trigger '{}' failed: {:#}", trigger.name, e)).into()
                })?;
                if let Some(reason) = output.get("reject") {
                    self.trigger_status.lock().unwrap().entry(trigger.name.clone()).or_default().rejected += 1;
                    let reason = reason.as_str().map(String::from).unwrap_or_else(|| reason.to_string());
                    bail!(EngineError::TriggerRejected(format!("Trigger '{}' rejected the write: {}", trigger.name, reason)));
                }
                if let (Some(Value::Object(fields)), crate::wal::WalOp::Put(doc)) = (output.get("document"), &mut item.op) {
                    let mut fields = fields.clone();
                    for reserved in crate::document::RESERVED_FIELDS {
                        if *reserved != "_schema" {
                            fields.remove(*reserved);
                        }
                    }
                    if let Some(stamp) = doc.data.get(crate::schema::SCHEMA_FIELD).cloned() {
                        fields.insert(crate::schema::SCHEMA_FIELD.into(), stamp.to_json());
                    }
                    doc.data = crate::document::infer_fields_from_json(&Value::Object(fields));
                }
            }
        }
        Ok(())
    }

    /// Run the after triggers a committed change matches.
    async fn run_after_triggers(&self, change: &Change) {
        if change.origin.is_some() || change.tessellation.starts_with('_') || self.is_system_tessellation(&change.tessellation) {
            return;
        }
        let event = change.event();
        if event == "drop_tessellation" {
            return;
        }
        for trigger in self.triggers_on(&change.tessellation, Timing::After).await {
            // Deletes carry no document here, so a filtered after trigger doesn't see them.
            if !trigger.applies(&change.tessellation, event, change.document.as_ref()) {
                continue;
            }
            let Some(id) = change.id else { continue };
            let input = payload(&trigger, event, &change.tessellation, id, change.document.as_ref(), None, None);
            if let Err(e) = self.run_trigger(&trigger, input).await {
                warn!("⚠️ Trigger '{}' failed on {} {} in '{}': {:#}", trigger.name, event, id, change.tessellation, e);
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedCursor {
    history_id: String,
    seq: u64,
}

fn cursor_id() -> Ulid {
    Ulid::from(u128::from_be_bytes(blake3::derive_key("HexDB 2026 trigger cursor v1", b"after").as_slice()[..16].try_into().unwrap()))
}

/// Run after triggers on the Overseer, following the change feed from the
/// saved position (at-least-once: a change may run again after a crash).
pub fn spawn_trigger_runner(engine: Arc<HexDBEngine>, mut shutdown_rx: watch::Receiver<()>) {
    tokio::spawn(async move {
        let mut reader: Option<crate::engine::ChangeReader> = None;
        let mut saved = 0u64;
        // The last change to a user tessellation that has been handled. System
        // changes (including this runner's own cursor saves) don't move it, so
        // saving the cursor doesn't cause another save.
        let mut handled = 0u64;
        let mut last_save = std::time::Instant::now();
        loop {
            if !engine.is_writable() {
                reader = None;
                tokio::select! {
                    _ = shutdown_rx.changed() => break,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => continue,
                }
            }
            if reader.is_none() {
                let end = engine.changes.published_seq();
                let start = match engine.get_system_document(TRIGGER_CURSORS_TESSELLATION, cursor_id()).await.ok().flatten() {
                    Some(doc) => match serde_json::from_value::<SavedCursor>(doc.data_json()) {
                        Ok(c) if c.history_id == engine.history_id() && c.seq >= engine.history_available_after() && c.seq <= end => c.seq,
                        _ => end,
                    },
                    None => end,
                };
                debug!("Trigger runner starts after sequence {}.", start);
                saved = start;
                handled = start;
                reader = Some(crate::engine::ChangeReader::new(start));
            }
            let r = reader.as_mut().unwrap();
            let next = tokio::select! {
                _ = shutdown_rx.changed() => break,
                next = tokio::time::timeout(Duration::from_secs(1), r.next(&engine)) => next,
            };
            match next {
                Ok(Some(change)) => {
                    engine.run_after_triggers(&change).await;
                    if !change.tessellation.starts_with('_') {
                        handled = change.seq;
                    }
                }
                Ok(None) => {
                    reader = None;
                    continue;
                }
                Err(_) => {}
            }
            if handled > saved && last_save.elapsed() >= Duration::from_secs(1) {
                let cursor = SavedCursor { history_id: engine.history_id(), seq: handled };
                if engine.put_system_document(TRIGGER_CURSORS_TESSELLATION, cursor_id(), json!(cursor), None).await.is_ok() {
                    saved = handled;
                }
                last_save = std::time::Instant::now();
            }
        }
        if reader.is_some() && handled > saved {
            let cursor = SavedCursor { history_id: engine.history_id(), seq: handled };
            let _ = engine.put_system_document(TRIGGER_CURSORS_TESSELLATION, cursor_id(), json!(cursor), None).await;
        }
        info!("🛑 Trigger runner stopped.");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_are_validated_and_match_events_and_filters() {
        let t: Trigger = serde_json::from_value(json!({ "name": "big-orders", "tessellation": "orders", "events": ["insert"], "function": "notify", "filter": { "total": { "$gt": 100 } } })).unwrap();
        t.validate().unwrap();
        assert_eq!(t.timing, Timing::After);
        let doc = |total: i64| Document { id: Ulid::new(), tessellation: "orders".into(), data: crate::document::infer_fields_from_json(&json!({ "total": total })), ttl: None };
        assert!(t.applies("orders", "insert", Some(&doc(500))));
        assert!(!t.applies("orders", "insert", Some(&doc(5))));
        assert!(!t.applies("orders", "update", Some(&doc(500))));
        assert!(!t.applies("customers", "insert", Some(&doc(500))));
        let bad: Trigger = serde_json::from_value(json!({ "name": "x", "tessellation": "orders", "events": ["upsert"], "function": "f" })).unwrap();
        assert!(bad.validate().is_err());
    }
}
