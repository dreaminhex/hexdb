// HexDB Core Engine: durable change history
//
// The change feed keeps the most recent changes in memory. Older changes are
// read back from the WAL archive (see `wal`), so a consumer that was away
// (a replica, a plugin, a `/changes` client) can resume from its cursor
// instead of starting over, as long as the archive still reaches back that far
// (`storage.change_history_hours` and `change_history_mb`).

use super::HexDBEngine;
use crate::{
    changes::{Change, ChangeKind, HistoryExpired},
    wal::{self, WalOp},
};
use chrono::{TimeZone, Utc};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tracing::warn;

impl HexDBEngine {
    /// True if retired WAL segments are archived as change history.
    pub(crate) fn keeps_change_history(&self) -> bool {
        self.live().change_history_hours > 0
    }

    /// Up to `limit` changes after `after`, oldest first: from memory when the
    /// feed still holds them, otherwise from the archive. Callers page by
    /// passing the last change's `seq`; an empty result means caught up.
    pub async fn changes_after(&self, after: u64, limit: usize) -> Result<Vec<Arc<Change>>, HistoryExpired> {
        match self.changes.since(after, limit) {
            Ok(changes) => Ok(changes),
            Err(expired) => self.changes_from_disk(after, expired.available_after, limit).await,
        }
    }

    /// Changes in `(after, up_to]` from the WAL archive and live segments.
    async fn changes_from_disk(&self, after: u64, up_to: u64, limit: usize) -> Result<Vec<Arc<Change>>, HistoryExpired> {
        let dir = self.wal_dir.clone();
        let keys = self.keys.clone();
        let read = tokio::task::spawn_blocking(move || {
            let floor = wal::history_floor(&dir).ok().flatten();
            (wal::read_history(&dir, &keys, after, up_to, limit), floor)
        })
        .await;
        let (result, floor) = match read {
            Ok(r) => r,
            Err(e) => {
                warn!("⚠️ Reading the change history failed: {}", e);
                return Err(HistoryExpired { available_after: up_to });
            }
        };
        let ops = match result {
            Ok(Some(ops)) => ops,
            Ok(None) => return Err(HistoryExpired { available_after: floor.unwrap_or(up_to).max(after).min(up_to) }),
            Err(e) => {
                warn!("⚠️ Reading the change history failed: {:#}", e);
                return Err(HistoryExpired { available_after: up_to });
            }
        };
        let mut changes: Vec<Change> = ops
            .into_iter()
            .filter_map(|h| {
                let timestamp = Utc.timestamp_millis_opt(h.time).single().unwrap_or_else(Utc::now);
                let (kind, tessellation, id, document) = match h.op {
                    WalOp::Put(doc) => (ChangeKind::Put, doc.tessellation.clone(), Some(doc.id), Some(doc)),
                    WalOp::Delete { tessellation, id } => (ChangeKind::Delete, tessellation, Some(id), None),
                    WalOp::Batch(_) => return None,
                };
                Some(Change { seq: h.seq, timestamp, kind, tessellation, id, document })
            })
            .collect();
        // Tessellation drops aren't WAL records; the catalog remembers when they happened.
        let last = changes.last().map(|c| c.seq).unwrap_or(up_to);
        let drops: Vec<(String, u64)> = {
            let catalog = self.catalog.lock().unwrap();
            catalog.dropped.iter().filter(|(_, seq)| **seq > after && **seq <= last).map(|(n, s)| (n.clone(), *s)).collect()
        };
        for (tessellation, seq) in drops {
            changes.push(Change { seq, timestamp: Utc::now(), kind: ChangeKind::DropTessellation, tessellation, id: None, document: None });
        }
        changes.sort_by_key(|c| c.seq);
        changes.truncate(limit);
        Ok(changes.into_iter().map(Arc::new).collect())
    }

    /// Delete archived history beyond the configured limits.
    pub(crate) async fn prune_change_history(&self) {
        if !self.keeps_change_history() {
            return;
        }
        let dir = self.wal_dir.clone();
        let keys = self.keys.clone();
        let live = self.live();
        let max_age = std::time::Duration::from_secs(live.change_history_hours * 3600);
        let max_bytes = live.change_history_mb.saturating_mul(1024 * 1024);
        match tokio::task::spawn_blocking(move || wal::prune_archive(&dir, &keys, max_age, max_bytes)).await {
            Ok(Ok(0)) => {}
            Ok(Ok(n)) => tracing::debug!("🗂️ Pruned {} archived WAL segment(s) from the change history.", n),
            Ok(Err(e)) => warn!("⚠️ Pruning the change history failed: {:#}", e),
            Err(e) => warn!("⚠️ Pruning the change history failed: {}", e),
        }
    }

    /// The oldest sequence number the change history can resume after.
    pub fn history_available_after(&self) -> u64 {
        let memory = self.changes.available_after();
        match wal::history_floor(&self.wal_dir) {
            Ok(Some(floor)) => floor.min(memory),
            _ => memory,
        }
    }
}

/// The change feed from a position: the history on disk first, then live.
/// Falling behind the live feed re-reads from the history instead of losing changes.
pub struct ChangeReader {
    /// The last change returned (or the starting position).
    pub cursor: u64,
    backlog: VecDeque<Arc<Change>>,
    receiver: Option<tokio::sync::broadcast::Receiver<Arc<Change>>>,
    /// Change positions that couldn't be delivered because the history no longer had them.
    pub skipped: u64,
}

impl ChangeReader {
    /// Read changes after `cursor`.
    pub fn new(cursor: u64) -> Self {
        ChangeReader { cursor, backlog: VecDeque::new(), receiver: None, skipped: 0 }
    }

    /// The next change, or None if the feed closed.
    pub async fn next(&mut self, engine: &HexDBEngine) -> Option<Arc<Change>> {
        loop {
            if let Some(change) = self.backlog.pop_front() {
                if change.seq <= self.cursor {
                    continue;
                }
                self.cursor = change.seq;
                return Some(change);
            }
            if self.receiver.is_none() {
                if self.cursor < engine.changes.available_after() {
                    match engine.changes_after(self.cursor, 1000).await {
                        Ok(page) if !page.is_empty() => {
                            self.backlog.extend(page);
                            continue;
                        }
                        Ok(_) => {}
                        Err(expired) => {
                            self.skipped += expired.available_after.saturating_sub(self.cursor);
                            self.cursor = expired.available_after;
                            continue;
                        }
                    }
                }
                match engine.changes.follow(self.cursor) {
                    Ok((backlog, receiver)) => {
                        self.backlog.extend(backlog);
                        self.receiver = Some(receiver);
                    }
                    Err(expired) => {
                        // Rolled past memory meanwhile; read from disk again.
                        if engine.changes_after(self.cursor, 1).await.is_err() {
                            self.skipped += expired.available_after.saturating_sub(self.cursor);
                            self.cursor = expired.available_after;
                        }
                    }
                }
                continue;
            }
            match self.receiver.as_mut().unwrap().recv().await {
                Ok(change) => {
                    if change.seq > self.cursor {
                        self.cursor = change.seq;
                        return Some(change);
                    }
                }
                // Fell behind the live feed: catch up from the history.
                Err(RecvError::Lagged(_)) => self.receiver = None,
                Err(RecvError::Closed) => return None,
            }
        }
    }

    /// A change that is already waiting, without blocking.
    pub fn ready(&mut self) -> Option<Arc<Change>> {
        loop {
            let change = match self.backlog.pop_front() {
                Some(change) => change,
                None => self.receiver.as_mut()?.try_recv().ok()?,
            };
            if change.seq > self.cursor {
                self.cursor = change.seq;
                return Some(change);
            }
        }
    }
}
