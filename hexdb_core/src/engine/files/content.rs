// File content and synchronous replication (`replication.min_acks`).
//
// A replica applies a file's document with the rest of the change feed and
// copies the file's content (its chunks) afterwards, in the background (see
// `run_file_sync`). Its position in the feed therefore says nothing about the
// bytes: an upload acknowledged because a replica had applied its document
// could still lose its content if the Overseer failed before the copy.
//
// So a replica also keeps a content position: the sequence number up to which
// it holds the content of every file it has applied. It sends it to the
// Overseer (`POST /lattice/content`) whenever it moves, and a write that
// touches a files tessellation waits for replicas whose content position has
// reached it, not just their feed position. A replica that never reports one
// (2.4.6 and earlier) counts by its feed position, as before.

use crate::blobs::ChunkId;
use crate::engine::HexDBEngine;
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tracing::debug;

/// How often a replica re-sends an unchanged content position (the Overseer
/// forgets positions it hasn't heard of for 30 seconds).
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// A replica's content position (see the module notes).
#[derive(Default)]
pub struct ContentProgress {
    inner: Mutex<ContentInner>,
    changed: tokio::sync::Notify,
}

#[derive(Default)]
struct ContentInner {
    /// The history the positions count in (the Overseer's).
    source: String,
    /// The last feed position applied.
    applied: u64,
    /// Content still being copied, by the feed position whose batch brought it.
    pending: BTreeMap<u64, HashSet<ChunkId>>,
}

impl ContentInner {
    fn position(&self) -> u64 {
        match self.pending.keys().next() {
            Some(first) => first.saturating_sub(1).min(self.applied),
            None => self.applied,
        }
    }
}

impl HexDBEngine {
    /// A replica applied the feed up to `seq` (of `source`'s history), bringing
    /// files whose content is `manifests`: hold the content position before
    /// `seq` until they're copied. Called before they're queued for copying.
    pub(crate) fn track_content(&self, source: &str, seq: u64, manifests: &[ChunkId]) {
        let progress = &self.file_sync.content;
        let mut inner = progress.inner.lock().unwrap();
        if inner.source != source {
            // Another history: positions in the old one mean nothing here.
            inner.source = source.to_string();
            inner.pending.clear();
        }
        inner.applied = seq;
        if !manifests.is_empty() {
            inner.pending.entry(seq).or_default().extend(manifests.iter().copied());
        }
        drop(inner);
        progress.changed.notify_one();
    }

    /// A file's content is here now.
    pub(crate) fn content_copied(&self, manifest: ChunkId) {
        let progress = &self.file_sync.content;
        let mut inner = progress.inner.lock().unwrap();
        let before = inner.position();
        for waiting in inner.pending.values_mut() {
            waiting.remove(&manifest);
        }
        inner.pending.retain(|_, waiting| !waiting.is_empty());
        let moved = inner.position() != before;
        drop(inner);
        if moved {
            progress.changed.notify_one();
        }
    }

    /// Stop waiting for content no document refers to any more (a file
    /// deleted or replaced before its content was copied: no hex keeps it).
    pub(crate) fn content_still_referenced(&self, referenced: &HashSet<ChunkId>) {
        let progress = &self.file_sync.content;
        let mut inner = progress.inner.lock().unwrap();
        let before = inner.position();
        for waiting in inner.pending.values_mut() {
            waiting.retain(|m| referenced.contains(m));
        }
        inner.pending.retain(|_, waiting| !waiting.is_empty());
        let moved = inner.position() != before;
        drop(inner);
        if moved {
            progress.changed.notify_one();
        }
    }

    /// (history, position): every file applied up to the position has its content here.
    pub fn content_position(&self) -> (String, u64) {
        let inner = self.file_sync.content.inner.lock().unwrap();
        (inner.source.clone(), inner.position())
    }

    /// On a replica, send the content position to the Overseer whenever it
    /// moves, and every `REPORT_EVERY` so it isn't forgotten.
    pub async fn run_content_reports(self: Arc<Self>, mut shutdown: watch::Receiver<()>) {
        let mut sent: Option<(String, u64)> = None;
        let mut last_sent = tokio::time::Instant::now();
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = self.file_sync.content.changed.notified() => {}
                _ = tokio::time::sleep(REPORT_EVERY) => {}
            }
            if self.is_writable() {
                sent = None;
                continue;
            }
            let position = self.content_position();
            if position.0.is_empty() || (sent.as_ref() == Some(&position) && last_sent.elapsed() < REPORT_EVERY) {
                continue;
            }
            let body = serde_json::json!({ "hex": self.id.to_string(), "source_id": position.0, "seq": position.1 });
            match tokio::time::timeout(Duration::from_secs(3), crate::replication::post_to_overseer(&self, "/lattice/content", &body)).await {
                Ok(Ok(())) => {
                    sent = Some(position);
                    last_sent = tokio::time::Instant::now();
                }
                Ok(Err(e)) => debug!("Couldn't send the content position to the Overseer: {:#}", e),
                Err(_) => debug!("Sending the content position to the Overseer timed out."),
            }
        }
    }

    /// On the Overseer: replica `hex` holds the content of every file up to
    /// `seq` of this hex's history.
    pub fn record_content_progress(&self, hex: &str, source_id: &str, seq: u64) {
        if hex.is_empty() || hex.len() > 64 || source_id != self.history_id() {
            return;
        }
        let mut progress = self.replica_progress.lock().unwrap();
        // Only for replicas following the feed (they report their feed position by polling it).
        if let Some(p) = progress.get_mut(hex) {
            p.content_seq = Some(seq);
        }
        drop(progress);
        self.progress_notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> ChunkId {
        [n; 32]
    }

    #[test]
    fn the_position_waits_for_the_oldest_content_still_being_copied() {
        let mut inner = ContentInner { source: "h".into(), ..Default::default() };
        inner.applied = 10;
        assert_eq!(inner.position(), 10);
        inner.pending.entry(12).or_default().insert(id(1));
        inner.pending.entry(15).or_default().insert(id(2));
        inner.applied = 20;
        assert_eq!(inner.position(), 11, "content brought at 12 isn't here yet");
        inner.pending.remove(&12);
        assert_eq!(inner.position(), 14);
        inner.pending.clear();
        assert_eq!(inner.position(), 20);
    }
}
