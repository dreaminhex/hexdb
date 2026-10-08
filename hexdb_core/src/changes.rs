// HexDB Core Change Feed
//
// Every committed write becomes a `Change`, published in sequence order once
// it is durable. Recent changes are kept in memory so consumers can catch up
// from a sequence number; live consumers subscribe to a broadcast channel.
//
// Consumers: the public `/changes` API (polling, long-polling, and Server-Sent
// Events), lattice replication, and plugins.
//
// Ordering: commits get consecutive sequence numbers under the engine lock but
// finish (become durable) independently, so a commit can complete before an
// earlier one. Completed ranges wait in `pending` until everything before them
// has completed, and are then published together, in order.

use crate::document::Document;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use ulid::Ulid;

/// Changes kept in memory for catch-up.
pub const CHANGE_HISTORY: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// A document was inserted or updated; `document` holds the new version.
    Put,
    /// A document was deleted.
    Delete,
    /// A whole tessellation was deleted.
    DropTessellation,
}

/// One committed change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub seq: u64,
    pub timestamp: DateTime<Utc>,
    pub kind: ChangeKind,
    pub tessellation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Ulid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<Document>,
    /// A put that created the document (an insert) rather than replacing it.
    #[serde(default)]
    pub created: bool,
    /// The trigger whose run made this change, if any (triggers don't fire on these).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl Change {
    /// "insert", "update", "delete" or "drop_tessellation".
    pub fn event(&self) -> &'static str {
        match self.kind {
            ChangeKind::Put if self.created => "insert",
            ChangeKind::Put => "update",
            ChangeKind::Delete => "delete",
            ChangeKind::DropTessellation => "drop_tessellation",
        }
    }

    /// The change as the public API shows it (documents as plain JSON).
    pub fn to_api_json(&self) -> Value {
        let mut value = json!({
            "seq": self.seq,
            "timestamp": self.timestamp,
            "op": self.kind,
            "event": self.event(),
            "tessellation": self.tessellation,
        });
        if let Some(origin) = &self.origin {
            value["trigger"] = Value::String(origin.clone());
        }
        if let Some(id) = self.id {
            value["id"] = Value::String(id.to_string());
        }
        if let Some(doc) = &self.document {
            value["document"] = doc.to_api_json();
        }
        value
    }
}

/// Requested history is no longer in memory; the consumer must resynchronize
/// (re-read the documents it needs, or take a replication snapshot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryExpired {
    /// Changes after this sequence number are available.
    pub available_after: u64,
}

/// A backlog of changes plus a receiver for the ones published after it.
pub type FollowResult = Result<(Vec<Arc<Change>>, broadcast::Receiver<Arc<Change>>), HistoryExpired>;

pub struct ChangeFeed {
    inner: Mutex<FeedInner>,
    sender: broadcast::Sender<Arc<Change>>,
}

struct FeedInner {
    ring: VecDeque<Arc<Change>>,
    capacity: usize,
    /// Completed commits waiting for earlier ones: first seq -> (count, changes).
    pending: BTreeMap<u64, (u64, Vec<Change>)>,
    /// The next sequence number to publish.
    next: u64,
    /// Every change after this sequence number is still in `ring`.
    floor: u64,
}

impl ChangeFeed {
    /// A feed whose first change will have sequence number `next_seq`.
    pub fn new(next_seq: u64, capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(4096);
        ChangeFeed {
            inner: Mutex::new(FeedInner {
                ring: VecDeque::new(),
                capacity: capacity.max(1),
                pending: BTreeMap::new(),
                next: next_seq,
                floor: next_seq.saturating_sub(1),
            }),
            sender,
        }
    }

    /// Record that sequence numbers `first..first+count` have committed,
    /// producing `changes` (possibly fewer, e.g. none after a failed write).
    pub(crate) fn complete(&self, first: u64, count: u64, changes: Vec<Change>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.pending.insert(first, (count, changes));
        loop {
            let next = inner.next;
            let Some((count, changes)) = inner.pending.remove(&next) else { break };
            inner.next = next + count.max(1);
            for change in changes {
                let change = Arc::new(change);
                if inner.ring.len() == inner.capacity {
                    if let Some(evicted) = inner.ring.pop_front() {
                        inner.floor = evicted.seq;
                    }
                }
                inner.ring.push_back(change.clone());
                // No receivers is fine.
                let _ = self.sender.send(change);
            }
        }
    }

    /// Live changes from now on. Combine with `since` to catch up first.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Change>> {
        self.sender.subscribe()
    }

    /// Up to `limit` changes with sequence numbers after `after`, oldest first.
    pub fn since(&self, after: u64, limit: usize) -> Result<Vec<Arc<Change>>, HistoryExpired> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if after < inner.floor {
            return Err(HistoryExpired { available_after: inner.floor });
        }
        // The ring is ordered by seq; skip to the first change after `after`.
        let start = inner.ring.partition_point(|c| c.seq <= after);
        Ok(inner.ring.iter().skip(start).take(limit).cloned().collect())
    }

    /// Subscribe and read the backlog atomically, so no change is missed or
    /// seen twice: returns the backlog after `after` and a receiver for
    /// everything published later.
    pub fn follow(&self, after: u64) -> FollowResult {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Publishing happens under this lock, so nothing slips between the two.
        let receiver = self.sender.subscribe();
        if after < inner.floor {
            return Err(HistoryExpired { available_after: inner.floor });
        }
        let start = inner.ring.partition_point(|c| c.seq <= after);
        Ok((inner.ring.iter().skip(start).cloned().collect(), receiver))
    }

    /// Sequence number of the last published change position (everything up
    /// to and including it has been published).
    pub fn published_seq(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).next.saturating_sub(1)
    }

    /// Changes after this sequence number can be read with `since`.
    pub fn available_after(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).floor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(seq: u64) -> Change {
        Change { seq, timestamp: Utc::now(), kind: ChangeKind::Delete, tessellation: "t".into(), id: Some(Ulid::new()), document: None, created: false, origin: None }
    }

    fn seqs(changes: &[Arc<Change>]) -> Vec<u64> {
        changes.iter().map(|c| c.seq).collect()
    }

    #[test]
    fn publishes_in_order_even_when_commits_finish_out_of_order() {
        let feed = ChangeFeed::new(10, 100);
        let mut rx = feed.subscribe();
        feed.complete(12, 2, vec![change(12), change(13)]);
        assert!(feed.since(9, 10).unwrap().is_empty(), "waits for seq 10 and 11");
        feed.complete(11, 1, vec![change(11)]);
        assert!(feed.since(9, 10).unwrap().is_empty(), "still waits for seq 10");
        feed.complete(10, 1, vec![change(10)]);
        assert_eq!(seqs(&feed.since(9, 10).unwrap()), [10, 11, 12, 13]);
        assert_eq!(feed.published_seq(), 13);
        for expected in [10, 11, 12, 13] {
            assert_eq!(rx.try_recv().unwrap().seq, expected);
        }
    }

    #[test]
    fn ranges_without_changes_still_advance() {
        let feed = ChangeFeed::new(1, 100);
        feed.complete(1, 3, vec![]);
        feed.complete(4, 1, vec![change(4)]);
        assert_eq!(seqs(&feed.since(0, 10).unwrap()), [4]);
        assert_eq!(seqs(&feed.since(3, 10).unwrap()), [4]);
        assert!(feed.since(4, 10).unwrap().is_empty());
    }

    #[test]
    fn evicted_history_is_reported() {
        let feed = ChangeFeed::new(1, 3);
        for seq in 1..=5 {
            feed.complete(seq, 1, vec![change(seq)]);
        }
        assert_eq!(seqs(&feed.since(2, 10).unwrap()), [3, 4, 5]);
        assert_eq!(feed.since(1, 10).unwrap_err(), HistoryExpired { available_after: 2 });
        assert_eq!(seqs(&feed.since(3, 1).unwrap()), [4]);

        let (backlog, mut rx) = feed.follow(4).unwrap();
        assert_eq!(seqs(&backlog), [5]);
        feed.complete(6, 1, vec![change(6)]);
        assert_eq!(rx.try_recv().unwrap().seq, 6);
    }

    #[test]
    fn history_starts_at_startup() {
        let feed = ChangeFeed::new(50, 10);
        assert_eq!(feed.since(0, 10).unwrap_err(), HistoryExpired { available_after: 49 });
        assert!(feed.since(49, 10).unwrap().is_empty());
    }
}
