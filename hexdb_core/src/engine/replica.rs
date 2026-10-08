// HexDB Core Engine: replica support
//
// The Overseer side serves consistent snapshots (documents with their
// versions), tracks how far each replica has applied its changes (a replica's
// poll for changes after N acknowledges everything up to N), and can hold a
// write until `replication.min_acks` replicas have it. The replica side
// applies the Overseer's writes, bypassing the write guard, together with its
// replication cursor in one atomic batch so a crash can never leave the data
// and the cursor out of step.

use super::{writes::BatchItem, EngineError, HexDBEngine};
use crate::{
    catalog::Catalog,
    document::{Document, FieldValue},
    hex::DocKey,
    wal::WalOp,
};
use anyhow::Result;
use std::collections::HashMap;
use ulid::Ulid;

/// System tessellation holding this hex's replication cursor. Never replicated.
pub const REPLICATION_TESSELLATION: &str = "_replication";

/// A replica's reported position.
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub applied_seq: u64,
    pub seen: std::time::Instant,
}

/// Replicas that haven't polled for this long don't count as acknowledging.
const PROGRESS_STALE: std::time::Duration = std::time::Duration::from_secs(30);

tokio::task_local! {
    /// Set while bootstrapping (default roles, the first admin): those writes
    /// happen before any replica can exist, so they don't wait for acks.
    pub static SKIP_ACKS: bool;
}

/// Writes to these tessellations never wait for replica acknowledgements
/// (bookkeeping that a request shouldn't stall on).
const NO_ACK_TESSELLATIONS: &[&str] =
    &[crate::audit::AUDIT_TESSELLATION, crate::auth::THROTTLE_TESSELLATION, REPLICATION_TESSELLATION, crate::plugins::PLUGIN_CURSORS_TESSELLATION];

/// Fixed document ID of the cursor.
fn cursor_id() -> Ulid {
    Ulid::from(1u128)
}

/// One write received from the Overseer.
#[derive(Debug, Clone)]
pub enum ReplicaWrite {
    Put(Document),
    Delete { tessellation: String, id: Ulid },
}

impl ReplicaWrite {
    fn key(&self) -> DocKey {
        match self {
            ReplicaWrite::Put(doc) => DocKey::new(&doc.tessellation, doc.id),
            ReplicaWrite::Delete { tessellation, id } => DocKey::new(tessellation, *id),
        }
    }
}

/// How far this replica has applied the Overseer's changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaCursor {
    /// The Overseer's hex ID (a new ID means a new Overseer or a restart: resync).
    pub source_id: String,
    /// The Overseer sequence number applied up to.
    pub applied_seq: u64,
}

fn kind_for(name: &str) -> &'static str {
    if name.starts_with('_') {
        "system"
    } else {
        Catalog::default_kind(name)
    }
}

impl HexDBEngine {
    /// The ID of this data directory's change history (see `Catalog::history_id`).
    pub fn history_id(&self) -> String {
        self.history_id.clone()
    }

    /// Note that replica `hex` has applied everything up to `applied_seq`.
    pub fn record_replica_progress(&self, hex: &str, applied_seq: u64) {
        if hex.is_empty() || hex.len() > 64 {
            return;
        }
        let mut progress = self.replica_progress.lock().unwrap();
        if progress.len() > 1000 && !progress.contains_key(hex) {
            progress.retain(|_, p| p.seen.elapsed() < PROGRESS_STALE);
        }
        progress.insert(hex.to_string(), Progress { applied_seq, seen: std::time::Instant::now() });
        drop(progress);
        self.progress_notify.notify_waiters();
    }

    /// A replica's position, if it has polled recently.
    pub fn replica_applied(&self, hex: &str) -> Option<u64> {
        self.replica_progress.lock().unwrap().get(hex).filter(|p| p.seen.elapsed() < PROGRESS_STALE).map(|p| p.applied_seq)
    }

    /// Replicas (polled recently) that have applied `seq`.
    fn acks_for(&self, seq: u64) -> usize {
        self.replica_progress.lock().unwrap().values().filter(|p| p.seen.elapsed() < PROGRESS_STALE && p.applied_seq >= seq).count()
    }

    /// With `replication.min_acks`, wait until that many replicas have applied
    /// `seq`. The write is already committed either way.
    pub(crate) async fn wait_for_acks(&self, seq: u64, tessellations: &[&str]) -> anyhow::Result<()> {
        let live = self.live();
        let wanted = live.min_acks;
        if wanted == 0 || tessellations.iter().all(|t| NO_ACK_TESSELLATIONS.contains(t)) || SKIP_ACKS.try_with(|skip| *skip).unwrap_or(false) {
            return Ok(());
        }
        let timeout = std::time::Duration::from_millis(live.ack_timeout_ms.max(1));
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Register before checking so a report in between isn't missed.
            let notified = self.progress_notify.notified();
            let acks = self.acks_for(seq);
            if acks >= wanted {
                return Ok(());
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                let acks = self.acks_for(seq);
                if acks >= wanted {
                    return Ok(());
                }
                return Err(EngineError::ReplicationTimeout(format!(
                    "The write was saved on the Overseer, but only {} of the {} required replica(s) confirmed it within {} ms. It will still replicate; retrying with the same Idempotency-Key is safe.",
                    acks,
                    wanted,
                    timeout.as_millis()
                ))
                .into());
            }
        }
    }

    /// With `replication.quorum`, fail unless this hex sees enough of the
    /// lattice (itself included) to be sure it isn't cut off from a majority.
    pub(crate) fn check_quorum(&self) -> anyhow::Result<()> {
        let quorum = self.config.replication.quorum;
        if quorum <= 1 {
            return Ok(());
        }
        let visible = 1 + self.peers.try_lock().map(|peers| peers.iter().filter(|p| p.status == "active").count()).unwrap_or(quorum);
        if visible < quorum {
            return Err(EngineError::NoQuorum(format!(
                "This hex sees {} of the {} hexes needed to accept writes (replication.quorum). It may be cut off from the rest of the lattice.",
                visible, quorum
            ))
            .into());
        }
        Ok(())
    }

    /// Documents of a tessellation with their versions, in ID order, for
    /// replication snapshots. Includes system tessellations.
    pub async fn snapshot_page(&self, tess: &str, after: Option<Ulid>, limit: usize) -> Result<(Vec<(Document, u64)>, Option<Ulid>)> {
        let mut ids: Vec<Ulid> = self
            .versions(tess)
            .await
            .into_iter()
            .filter(|(id, v)| v.visible() && after.is_none_or(|a| *id > a))
            .map(|(id, _)| id)
            .collect();
        ids.sort();
        let mut out = Vec::new();
        let mut next = None;
        for id in ids {
            if out.len() == limit {
                next = out.last().map(|(d, _): &(Document, u64)| d.id);
                break;
            }
            let (doc, seq) = self.read_latest(&DocKey::new(tess, id)).await?;
            if let Some(doc) = doc {
                out.push((doc, seq));
            }
        }
        Ok((out, next))
    }

    /// IDs of the visible documents in a tessellation.
    pub(crate) async fn visible_ids(&self, tess: &str) -> Vec<Ulid> {
        self.versions(tess).await.into_iter().filter(|(_, v)| v.visible()).map(|(id, _)| id).collect()
    }

    /// Make sure a tessellation exists locally (replication may create user
    /// and system tessellations alike).
    pub(crate) fn ensure_replicated_tessellation(&self, name: &str) -> Result<()> {
        if self.tessellation_exists(name) {
            return Ok(());
        }
        match kind_for(name) {
            "system" if name.starts_with('_') => self.ensure_system_tessellation(name),
            kind => self.create_tessellation_unchecked(name, kind).map(|_| ()),
        }
    }

    /// Apply writes from the Overseer atomically, with the new cursor.
    pub(crate) async fn apply_replicated(&self, writes: Vec<ReplicaWrite>, cursor: Option<&ReplicaCursor>) -> Result<()> {
        // A document changed several times in one batch: keep its last write.
        let mut last: HashMap<DocKey, usize> = HashMap::new();
        for (i, w) in writes.iter().enumerate() {
            last.insert(w.key(), i);
        }
        let mut items = Vec::with_capacity(last.len() + 1);
        for (i, w) in writes.into_iter().enumerate() {
            let key = w.key();
            if last.get(&key) != Some(&i) {
                continue;
            }
            self.ensure_replicated_tessellation(&key.tessellation)?;
            let op = match w {
                ReplicaWrite::Put(doc) => WalOp::Put(doc),
                ReplicaWrite::Delete { tessellation, id } => WalOp::Delete { tessellation, id },
            };
            items.push(BatchItem { key, op, expected_seq: None });
        }
        if let Some(cursor) = cursor {
            self.ensure_system_tessellation(REPLICATION_TESSELLATION)?;
            let mut data = crate::document::CompactFields::new();
            data.insert("source_id".into(), FieldValue::String(cursor.source_id.clone()));
            data.insert("applied_seq".into(), FieldValue::Integer(cursor.applied_seq as i64));
            let doc = Document { id: cursor_id(), tessellation: REPLICATION_TESSELLATION.into(), data, ttl: None };
            items.push(BatchItem { key: DocKey::new(REPLICATION_TESSELLATION, doc.id), op: WalOp::Put(doc), expected_seq: None });
        }
        self.commit_replicated(items).await
    }

    /// The saved replication cursor, if this hex has been a replica.
    pub(crate) async fn load_replica_cursor(&self) -> Option<ReplicaCursor> {
        if !self.tessellation_exists(REPLICATION_TESSELLATION) {
            return None;
        }
        let (doc, _) = self.read_latest(&DocKey::new(REPLICATION_TESSELLATION, cursor_id())).await.ok()?;
        let data = doc?.data_json();
        Some(ReplicaCursor {
            source_id: data.get("source_id")?.as_str()?.to_string(),
            applied_seq: data.get("applied_seq")?.as_u64()?,
        })
    }

    /// Drop a tessellation because the Overseer did (or doesn't have it).
    pub(crate) async fn drop_replicated_tessellation(&self, name: &str) -> Result<()> {
        self.delete_tessellation_unchecked(name).await.map(|_| ())
    }

    /// Make this tessellation's indexes match the Overseer's definitions.
    pub(crate) async fn reconcile_indexes(&self, tess: &str, wanted: &[crate::index::IndexDef]) -> Result<()> {
        let have: Vec<crate::index::IndexDef> = self.list_indexes(tess).into_iter().map(|i| i.def).collect();
        for def in have.iter().filter(|d| !wanted.contains(d)) {
            self.drop_index_unchecked(tess, &def.name)?;
        }
        for def in wanted.iter().filter(|d| !have.contains(d)) {
            let created = if self.is_system_tessellation(tess) {
                self.ensure_internal_index(tess, def.clone()).await
            } else {
                self.create_index_unchecked(tess, def.clone()).await.map(|_| ())
            };
            if let Err(e) = created {
                tracing::warn!("⚠️ Couldn't replicate index '{}' on '{}': {:#}", def.name, tess, e);
            }
        }
        Ok(())
    }
}
