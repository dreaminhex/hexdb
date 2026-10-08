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
}

fn cursor_doc_id(plugin: &str) -> ulid::Ulid {
    let hash = blake3::derive_key("HexDB 2026 plugin cursor v1", plugin.as_bytes());
    ulid::Ulid::from(u128::from_be_bytes(hash[..16].try_into().unwrap()))
}

async fn load_cursor(engine: &HexDBEngine, plugin: &str) -> Option<SavedCursor> {
    let doc = engine.get_system_document(PLUGIN_CURSORS_TESSELLATION, cursor_doc_id(plugin)).await.ok()??;
    serde_json::from_value(doc.data_json()).ok()
}

pub(super) async fn save_cursor(engine: &HexDBEngine, plugin: &str, seq: u64) {
    let cursor = SavedCursor { plugin: plugin.to_string(), history_id: engine.history_id(), seq };
    let Ok(json) = serde_json::to_value(&cursor) else { return };
    if let Err(e) = engine.put_system_document(PLUGIN_CURSORS_TESSELLATION, cursor_doc_id(plugin), json, None).await {
        debug!(plugin = %plugin, "Couldn't save the plugin's position: {:#}", e);
    }
}

/// Where a plugin starts: its saved position, if it belongs to this history
/// and the history still reaches back to it; otherwise the end of the feed.
/// A new starting point is saved at once, so a plugin that fails before its
/// first delivery resumes from there instead of skipping to the end again.
async fn starting_position(engine: &HexDBEngine, plugin: &str) -> u64 {
    let end = engine.changes.published_seq();
    let start = match load_cursor(engine, plugin).await {
        Some(c) if c.history_id == engine.history_id() && c.seq >= engine.history_available_after() && c.seq <= end => {
            if c.seq < end {
                info!(plugin = %plugin, "🧩 Plugin '{}' resumes after sequence {} ({} change positions to catch up).", plugin, c.seq, end - c.seq);
            }
            return c.seq;
        }
        Some(c) if c.history_id != engine.history_id() => {
            warn!(plugin = %plugin, "⚠️ Plugin '{}' was following another Overseer's history; it starts at the end of this one.", plugin);
            end
        }
        Some(c) => {
            warn!(plugin = %plugin, "⚠️ Plugin '{}' was at sequence {}, older than the change history kept; changes up to {} are skipped.", plugin, c.seq, end);
            end
        }
        None => end,
    };
    save_cursor(engine, plugin, start).await;
    start
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
    Logs { after: u64, level: Option<tracing::Level>, targets: Vec<String> },
    Metrics { last: Option<chrono::DateTime<chrono::Utc>> },
    /// Source plugins receive nothing.
    Nothing,
}

impl Feed {
    pub async fn open(engine: &HexDBEngine, manifest: &Manifest) -> Feed {
        match manifest.kind() {
            PluginKind::Stream => Feed::Changes(ChangeReader::new(starting_position(engine, &manifest.id).await)),
            PluginKind::Logs => Feed::Logs {
                // Logs start now: history before the plugin started isn't sent.
                after: crate::logging::log_buffer().last_seq(),
                level: manifest.logs.level.as_deref().and_then(crate::logging::parse_level),
                targets: manifest.logs.targets.clone(),
            },
            PluginKind::Metrics => Feed::Metrics { last: engine.history.last().map(|s| s.timestamp) },
            PluginKind::Source => Feed::Nothing,
        }
    }

    /// The change feed position (stream plugins).
    pub fn position(&self) -> Option<u64> {
        match self {
            Feed::Changes(reader) => Some(reader.cursor),
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
            Feed::Nothing => std::future::pending().await,
        }
    }
}
