// What plugins receive: the change feed (with saved positions), the log, or
// metrics samples. Each feed yields JSON payloads in order.

use super::{Manifest, PluginKind, PLUGIN_CURSORS_TESSELLATION};
use crate::{
    changes::{Change, ChangeKind},
    engine::{ChangeReader, HexDBEngine},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use std::time::Duration;
use tracing::{debug, info, warn};

/// How often the log and metrics feeds look for new records.
const POLL_EVERY: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Change feed positions
// ---------------------------------------------------------------------------

/// A plugin's position in the change feed, saved in `_plugin_cursors`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedCursor {
    plugin: String,
    history_id: String,
    seq: u64,
    /// Why the plugin can't continue from `seq` (the history no longer reaches it): it waits for
    /// an administrator to choose how to resynchronize (see `resolve_resync`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resync: Option<String>,
    /// Deliver every current document of the plugin's tessellations first, then continue from `seq`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    snapshot: bool,
}

fn cursor_doc_id(plugin: &str) -> ulid::Ulid {
    let hash = blake3::derive_key("HexDB 2026 plugin cursor v1", plugin.as_bytes());
    ulid::Ulid::from(u128::from_be_bytes(hash[..16].try_into().unwrap()))
}

async fn load_cursor(engine: &HexDBEngine, plugin: &str) -> Option<SavedCursor> {
    let doc = engine.get_system_document(PLUGIN_CURSORS_TESSELLATION, cursor_doc_id(plugin)).await.ok()??;
    serde_json::from_value(doc.data_json()).ok()
}

async fn store_cursor(engine: &HexDBEngine, cursor: &SavedCursor) -> anyhow::Result<()> {
    engine.put_system_document(PLUGIN_CURSORS_TESSELLATION, cursor_doc_id(&cursor.plugin), serde_json::to_value(cursor)?, None).await
}

pub(super) async fn save_cursor(engine: &HexDBEngine, plugin: &str, seq: u64) {
    let cursor = SavedCursor { plugin: plugin.to_string(), history_id: engine.history_id(), seq, resync: None, snapshot: false };
    if let Err(e) = store_cursor(engine, &cursor).await {
        debug!(plugin = %plugin, "Couldn't save the plugin's position: {:#}", e);
    }
}

/// An administrator's answer to a plugin waiting for a resync: continue from the end of the
/// history now (the changes it missed stay missed), or (`snapshot`) receive every current
/// document of its tessellations first and then continue from here.
pub(crate) async fn resolve_resync(engine: &HexDBEngine, plugin: &str, snapshot: bool) -> anyhow::Result<()> {
    if load_cursor(engine, plugin).await.and_then(|c| c.resync).is_none() {
        anyhow::bail!(crate::engine::EngineError::Invalid(format!("Plugin '{}' isn't waiting for a resync.", plugin)));
    }
    let cursor = SavedCursor { plugin: plugin.to_string(), history_id: engine.history_id(), seq: engine.changes.published_seq(), resync: None, snapshot };
    store_cursor(engine, &cursor).await
}

/// Where a plugin may start, or why it can't continue from its saved position.
enum Start {
    /// Resume after this sequence (with a snapshot first), saving the position if it's new.
    At { seq: u64, snapshot: bool, save: bool },
    Gap(String),
}

fn decide(engine: &HexDBEngine, plugin: &str, saved: Option<&SavedCursor>) -> Start {
    let end = engine.changes.published_seq();
    let Some(c) = saved else { return Start::At { seq: end, snapshot: false, save: true } };
    if let Some(reason) = &c.resync {
        return Start::Gap(reason.clone());
    }
    if c.history_id == engine.history_id() {
        if c.seq >= engine.history_available_after() && c.seq <= end {
            if c.seq < end {
                info!(plugin = %plugin, "🧩 Plugin '{}' resumes after sequence {} ({} change positions to catch up).", plugin, c.seq, end - c.seq);
            }
            return Start::At { seq: c.seq, snapshot: c.snapshot, save: false };
        }
        return Start::Gap(format!("its position (sequence {}) is older than the change history kept", c.seq));
    }
    // A new Overseer that continued the old history knows where its own history holds the
    // old one up to the saved position (or a little before it: some changes come again).
    let continued = engine.continuation.read().unwrap().as_ref().and_then(|k| k.resume_at(&c.history_id, c.seq));
    match continued {
        Some(local) if local >= engine.history_available_after() && local <= end => {
            info!(plugin = %plugin, "🧩 Plugin '{}' continues from the previous Overseer's history (here after sequence {}).", plugin, local);
            Start::At { seq: local, snapshot: c.snapshot, save: true }
        }
        _ => Start::Gap("it was following another Overseer's history, which this one doesn't continue (or no longer keeps)".into()),
    }
}

/// Where a plugin starts: its saved position, if the history still reaches it; the end of the
/// feed for a new plugin. Otherwise it waits, delivering nothing, until an administrator chooses
/// how to resynchronize (`POST /plugins/{id}/resync`), rather than silently skipping what it
/// missed. `None` if this hex stops being the Overseer meanwhile. A new starting point is saved
/// at once, so a plugin that fails before its first delivery resumes from there.
async fn starting_position(engine: &HexDBEngine, plugin: &str) -> Option<(u64, bool)> {
    let mut warned = false;
    loop {
        let saved = load_cursor(engine, plugin).await;
        match decide(engine, plugin, saved.as_ref()) {
            Start::At { seq, snapshot, save } => {
                if save {
                    let cursor = SavedCursor { plugin: plugin.to_string(), history_id: engine.history_id(), seq, resync: None, snapshot };
                    if let Err(e) = store_cursor(engine, &cursor).await {
                        debug!(plugin = %plugin, "Couldn't save the plugin's position: {:#}", e);
                    }
                }
                if warned {
                    info!(plugin = %plugin, "🧩 Plugin '{}' was resynchronized: it continues after sequence {}{}.", plugin, seq, if snapshot { ", after a snapshot of its tessellations" } else { "" });
                    engine.plugins.update(plugin, |p| {
                        p.state = "running".into();
                        p.last_error = None;
                    });
                }
                return Some((seq, snapshot));
            }
            Start::Gap(reason) => {
                if !warned {
                    warn!(plugin = %plugin, "⚠️ Plugin '{}' can't continue: {}. It waits for an administrator to resynchronize it (POST /plugins/{}/resync).", plugin, reason, plugin);
                    if let Some(c) = saved.filter(|c| c.resync.is_none()) {
                        let _ = store_cursor(engine, &SavedCursor { resync: Some(reason.clone()), ..c }).await;
                    }
                    engine.plugins.update(plugin, |p| {
                        p.state = "needs_resync".into();
                        p.last_error = Some(reason);
                    });
                    warned = true;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                if !engine.is_writable() {
                    return None;
                }
            }
        }
    }
}

fn wanted(manifest: &Manifest, engine: &HexDBEngine, change: &Change) -> bool {
    if change.tessellation == crate::audit::AUDIT_TESSELLATION {
        return manifest.audit && change.kind == ChangeKind::Put;
    }
    if !manifest.tessellations.is_empty() && !manifest.tessellations.contains(&change.tessellation) {
        return false;
    }
    if change.kind == ChangeKind::DropTessellation {
        return !change.tessellation.starts_with('_');
    }
    !engine.is_system_tessellation(&change.tessellation)
}

// ---------------------------------------------------------------------------
// Feeds
// ---------------------------------------------------------------------------

/// One batch for a plugin.
pub(super) struct Batch {
    pub payloads: Vec<Value>,
    /// Log records, for the OTLP exporter.
    pub logs: Vec<crate::logging::LogRecord>,
    /// Metrics samples, for the OTLP exporter.
    pub samples: Vec<crate::metrics::MetricsSample>,
    /// The change feed position after this batch (stream plugins).
    pub position: Option<u64>,
}

pub(super) enum Feed {
    Changes(ChangeReader),
    /// Every current document of the plugin's tessellations (as `snapshot` events), then the changes.
    Snapshot { then: ChangeReader, tessellations: Vec<String>, after: Option<ulid::Ulid> },
    Logs { after: u64, level: Option<tracing::Level>, targets: Vec<String> },
    Metrics { last: Option<crate::clock::UtcTime> },
    /// Source plugins receive nothing.
    Nothing,
}

impl Feed {
    /// The plugin's feed; `None` if this hex stopped being the Overseer while the plugin waited.
    pub async fn open(engine: &HexDBEngine, manifest: &Manifest) -> Option<Feed> {
        Some(match manifest.kind() {
            PluginKind::Stream => {
                let (seq, snapshot) = starting_position(engine, &manifest.id).await?;
                let reader = ChangeReader::new(seq);
                if snapshot {
                    let mut tessellations: Vec<String> = engine
                        .tessellations()
                        .into_iter()
                        .map(|(name, _)| name)
                        .filter(|t| !engine.is_system_tessellation(t) && (manifest.tessellations.is_empty() || manifest.tessellations.contains(t)))
                        .collect();
                    tessellations.sort();
                    Feed::Snapshot { then: reader, tessellations, after: None }
                } else {
                    Feed::Changes(reader)
                }
            }
            PluginKind::Logs => Feed::Logs {
                // Logs start now: history before the plugin started isn't sent.
                after: crate::logging::log_buffer().last_seq(),
                level: manifest.logs.level.as_deref().and_then(crate::logging::parse_level),
                targets: manifest.logs.targets.clone(),
            },
            PluginKind::Metrics => Feed::Metrics { last: engine.history.last().map(|s| s.timestamp) },
            PluginKind::Source => Feed::Nothing,
        })
    }

    /// The change feed position (stream plugins).
    pub fn position(&self) -> Option<u64> {
        match self {
            Feed::Changes(reader) | Feed::Snapshot { then: reader, .. } => Some(reader.cursor),
            _ => None,
        }
    }

    /// Change positions skipped since the last call.
    pub fn take_skipped(&mut self) -> u64 {
        match self {
            Feed::Changes(reader) => std::mem::take(&mut reader.skipped),
            _ => 0,
        }
    }

    /// Wait for the next batch of up to `max` payloads. `None` when the feed ends.
    pub async fn next_batch(&mut self, engine: &HexDBEngine, manifest: &Manifest, max: usize) -> Option<Batch> {
        while let Feed::Snapshot { then, tessellations, after } = self {
            let Some(tess) = tessellations.first().cloned() else {
                let reader = std::mem::replace(then, ChangeReader::new(0));
                *self = Feed::Changes(reader);
                break;
            };
            let page = match engine.list_documents(&tess, *after, max.max(1)).await {
                Ok(page) => page,
                // Dropped meanwhile: nothing left to send of it.
                Err(_) => crate::engine::ListPage { documents: Vec::new(), next: None },
            };
            *after = page.next;
            if page.next.is_none() {
                tessellations.remove(0);
            }
            if page.documents.is_empty() {
                continue;
            }
            // At the snapshot's position: a plugin that fails during it gets the whole snapshot again.
            let seq = then.cursor;
            let now = crate::clock::UtcTime::now();
            let payloads = page
                .documents
                .iter()
                .map(|doc| {
                    serde_json::json!({
                        "seq": seq, "timestamp": now, "op": "put", "event": "snapshot",
                        "tessellation": tess, "id": doc.id.to_string(), "document": crate::sealed::unsealed(doc).to_api_json(),
                    })
                })
                .collect();
            return Some(Batch { payloads, logs: Vec::new(), samples: Vec::new(), position: Some(seq) });
        }
        match self {
            Feed::Changes(reader) => {
                let first = reader.next(engine).await?;
                let mut payloads = Vec::new();
                if wanted(manifest, engine, &first) {
                    payloads.push(first.to_api_json());
                }
                while payloads.len() < max {
                    let Some(more) = reader.ready() else { break };
                    if wanted(manifest, engine, &more) {
                        payloads.push(more.to_api_json());
                    }
                }
                Some(Batch { payloads, logs: Vec::new(), samples: Vec::new(), position: Some(reader.cursor) })
            }
            Feed::Logs { after, level, targets } => loop {
                let records = crate::logging::log_buffer().query(&crate::logging::LogQuery {
                    level: *level,
                    after: Some(*after),
                    before: None,
                    search: None,
                    target: None,
                    limit: max.max(1),
                });
                if let Some(last) = records.last() {
                    *after = last.seq;
                }
                let records: Vec<_> = records
                    .into_iter()
                    // A plugin's own output is logged under hexdb_core::plugins; sending it back would loop.
                    .filter(|r| !r.target.starts_with("hexdb_core::plugins"))
                    .filter(|r| targets.is_empty() || targets.iter().any(|t| r.target.starts_with(t.as_str())))
                    .collect();
                if !records.is_empty() {
                    let payloads = records.iter().filter_map(|r| serde_json::to_value(r).ok()).collect();
                    return Some(Batch { payloads, logs: records, samples: Vec::new(), position: None });
                }
                tokio::time::sleep(POLL_EVERY).await;
            },
            Feed::Metrics { last } => loop {
                let samples: Vec<_> = engine.history.all().into_iter().filter(|s| last.is_none_or(|t| s.timestamp > t)).take(max.max(1)).collect();
                if let Some(newest) = samples.last() {
                    *last = Some(newest.timestamp);
                    let payloads = samples.iter().filter_map(|s| serde_json::to_value(s).ok()).collect();
                    return Some(Batch { payloads, logs: Vec::new(), samples, position: None });
                }
                tokio::time::sleep(POLL_EVERY).await;
            },
            Feed::Snapshot { .. } => unreachable!("a snapshot becomes the change feed above"),
            Feed::Nothing => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod review_256 {
    use super::*;

    /// H256-03: a plugin's position in the previous Overseer's history carries over through the
    /// continuation instead of jumping to the end of the new history.
    #[tokio::test]
    async fn review256_plugin_cursor_must_use_available_failover_continuation() {
        let (engine, dir) = crate::engine::test_engine().await;
        for n in 0..3 {
            engine.insert_json("orders", serde_json::json!({ "n": n }), None).await.unwrap();
        }
        let local = engine.history_available_after().max(1);
        assert!(engine.changes.published_seq() > local, "changes after the continuation point");
        *engine.continuation.write().unwrap() = Some(crate::replication::Continuation { source_id: "old-history".into(), seq: 40, local_seq: local, earlier: Vec::new() });
        let saved = SavedCursor { plugin: "@test/sink".into(), history_id: "old-history".into(), seq: 40, resync: None, snapshot: false };
        engine.put_system_document(PLUGIN_CURSORS_TESSELLATION, cursor_doc_id("@test/sink"), serde_json::to_value(&saved).unwrap(), None).await.unwrap();
        let start = starting_position(&engine, "@test/sink").await;
        engine.shutdown().await.unwrap();
        std::fs::remove_dir_all(dir).ok();
        assert_eq!(start, Some((local, false)), "the plugin must resume where the continuation says, not at the end");
    }

    /// A plugin whose position the history no longer reaches waits for an administrator instead
    /// of skipping what it missed; resynchronized with a snapshot, it gets every current document
    /// of its tessellations, then the changes after the decision.
    #[tokio::test]
    async fn a_plugin_past_the_history_waits_for_a_resync_then_gets_a_snapshot() {
        let (engine, dir) = crate::engine::test_engine().await;
        let engine = std::sync::Arc::new(engine);
        for n in 0..3 {
            engine.insert_json("orders", serde_json::json!({ "n": n }), None).await.unwrap();
        }
        engine.insert_json("other", serde_json::json!({ "skip": true }), None).await.unwrap();
        let saved = SavedCursor { plugin: "@test/sink".into(), history_id: "unknown-history".into(), seq: 7, resync: None, snapshot: false };
        store_cursor(&engine, &saved).await.unwrap();
        assert!(resolve_resync(&engine, "@test/other", false).await.is_err(), "only a waiting plugin");

        let waiting = tokio::spawn({
            let engine = engine.clone();
            async move { starting_position(&engine, "@test/sink").await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!waiting.is_finished(), "it waits rather than skipping");
        assert!(load_cursor(&engine, "@test/sink").await.unwrap().resync.is_some(), "the wait is saved");
        let at_decision = engine.changes.published_seq();
        resolve_resync(&engine, "@test/sink", true).await.unwrap();
        let (seq, snapshot) = waiting.await.unwrap().unwrap();
        assert!(snapshot);
        assert_eq!(seq, at_decision, "continues from where the history was when the administrator decided");

        let manifest: Manifest = serde_json::from_value(serde_json::json!({ "id": "@test/sink", "name": "sink", "tessellations": ["orders"] })).unwrap();
        let mut feed = Feed::Snapshot { then: ChangeReader::new(seq), tessellations: vec!["orders".into()], after: None };
        let batch = feed.next_batch(&engine, &manifest, 2).await.unwrap();
        let second = feed.next_batch(&engine, &manifest, 2).await.unwrap();
        let mut ns: Vec<i64> = batch.payloads.iter().chain(&second.payloads).map(|p| p["document"]["n"].as_i64().unwrap()).collect();
        ns.sort();
        assert_eq!(ns, [0, 1, 2]);
        assert!(batch.payloads.iter().all(|p| p["event"] == "snapshot" && p["seq"] == seq));
        // Then the changes after the decision.
        engine.insert_json("orders", serde_json::json!({ "n": 3 }), None).await.unwrap();
        // (Batches of system changes, such as the plugin's own position, carry nothing.)
        let next = loop {
            let batch = tokio::time::timeout(Duration::from_secs(5), feed.next_batch(&engine, &manifest, 10)).await.unwrap().unwrap();
            if !batch.payloads.is_empty() {
                break batch;
            }
        };
        assert_eq!(next.payloads[0]["document"]["n"], 3);
        assert_eq!(next.payloads[0]["event"], "insert");
        engine.shutdown().await.unwrap();
        std::fs::remove_dir_all(dir).ok();
    }
}
