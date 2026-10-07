// HexDB Core Engine: replica support
//
// The Overseer side serves consistent snapshots (documents with their
// versions); the replica side applies the Overseer's writes, bypassing the
// write guard, together with its replication cursor in one atomic batch so a
// crash can never leave the data and the cursor out of step.

use super::{writes::BatchItem, HexDBEngine};
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
            if let Err(e) = self.create_index_unchecked(tess, def.clone()).await {
                tracing::warn!("⚠️ Couldn't replicate index '{}' on '{}': {:#}", def.name, tess, e);
            }
        }
        Ok(())
    }
}
