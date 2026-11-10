// HexDB Core Engine: sealed fields
//
// Applies `crate::sealed` to what the engine keeps in memory: writes are
// sealed before they reach the WAL, the hex and the change feed; documents
// read from disk are sealed again before the hex caches them; and when roles
// change which fields are masked, everything the hex holds is resealed.

use super::{
    deletes::{TRASHED_DOCUMENT, TRASH_TESSELLATION},
    writes::BatchItem,
    HexDBEngine,
};
use crate::{
    hex::{DocKey, Lookup},
    sealed::SealPolicy,
    sst::parse_document,
    wal::WalOp,
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::Arc;
use tracing::{info, warn};

impl HexDBEngine {
    /// The fields of `tess` that are kept sealed (none for system tessellations).
    pub(crate) fn sealed_paths(&self, tess: &str) -> Vec<String> {
        if self.is_system_tessellation(tess) {
            return Vec::new();
        }
        self.seal_policy.read().unwrap().paths(tess)
    }

    /// The sealed fields of one document: its tessellation's, or for a trash
    /// entry, those of the tessellation it was deleted from, under `document.`.
    pub(crate) fn doc_sealed_paths(&self, doc: &crate::Document) -> Vec<String> {
        trash_paths(doc, |tess| self.sealed_paths(tess)).unwrap_or_else(|| self.sealed_paths(&doc.tessellation))
    }

    /// Seal the documents a commit writes.
    pub(crate) fn seal_items(&self, items: &mut [BatchItem]) {
        if self.seal_policy.read().unwrap().is_empty() {
            return;
        }
        for item in items {
            if let WalOp::Put(doc) = &mut item.op {
                let paths = self.doc_sealed_paths(doc);
                if !paths.is_empty() {
                    *doc = self.sealer.seal_document(doc, &paths);
                }
            }
        }
    }

    /// Seal the documents inside a stored result (idempotency records keep
    /// the documents a request returned, as `{id, tessellation, json}`).
    pub(crate) fn seal_nested_documents(&self, value: &mut Value) {
        if self.seal_policy.read().unwrap().is_empty() {
            return;
        }
        match value {
            Value::Array(items) => items.iter_mut().for_each(|v| self.seal_nested_documents(v)),
            Value::Object(map) => {
                let place = (map.get("tessellation").and_then(Value::as_str), map.get("id").and_then(Value::as_str));
                if let ((Some(tess), Some(id)), true) = (place, map.contains_key("json")) {
                    let (tess, id) = (tess.to_string(), id.to_string());
                    let paths = self.sealed_paths(&tess);
                    if let Some(json) = map.get_mut("json") {
                        self.sealer.seal_json(&tess, &id, json, &paths);
                    }
                } else {
                    map.values_mut().for_each(|v| self.seal_nested_documents(v));
                }
            }
            _ => {}
        }
    }

    /// Stored bytes as the hex keeps them: with this tessellation's sealed
    /// fields sealed (documents in SSTables may predate the mask).
    pub(crate) fn sealed_bytes(&self, doc: &crate::Document, bytes: Vec<u8>) -> Vec<u8> {
        let paths = self.doc_sealed_paths(doc);
        if paths.is_empty() {
            return bytes;
        }
        serde_json::to_vec(&self.sealer.seal_document(doc, &paths)).unwrap_or(bytes)
    }

    /// Read which fields roles mask and, if that changed, reseal what this
    /// hex holds in memory and rebuild the affected indexes. Runs at startup
    /// and after every write to roles (here or replicated).
    pub async fn refresh_seal_policy(&self) {
        let policy = if self.tessellation_exists(crate::users::ROLES_TESSELLATION) {
            match crate::users::role_definitions(self).await {
                Ok(definitions) => SealPolicy::from_roles(&definitions),
                Err(e) => {
                    warn!("⚠️ Couldn't read the roles to find the masked fields to seal: {:#}", e);
                    return;
                }
            }
        } else {
            SealPolicy::default()
        };
        let old = self.seal_policy.read().unwrap().clone();
        // The first run after startup also seals what WAL replay put in the hex.
        let first = self.seal_pending.swap(false, std::sync::atomic::Ordering::SeqCst);
        if *old == policy && !first {
            return;
        }
        crate::sealed::register_wipe_paths(&policy);
        *self.seal_policy.write().unwrap() = Arc::new(policy.clone());

        // Every tessellation the hex, the indexes or the change feed has whose sealed fields changed.
        let mut affected: BTreeSet<String> = self.state.lock().await.hex.keys().into_iter().map(|k| k.tessellation).collect();
        affected.extend(self.indexes.read().unwrap().by_tessellation.keys().cloned());
        affected.retain(|t| !self.is_system_tessellation(t) && (policy.differs_for(&old, t) || (first && !policy.paths(t).is_empty())));
        // Trash entries are sealed by the tessellation they came from: reseal them all.
        let trash = *old != policy || first;
        if affected.is_empty() && !trash {
            return;
        }
        let paths_of = |doc: &crate::Document| trash_paths(doc, |t| policy.paths(t)).unwrap_or_else(|| policy.paths(&doc.tessellation));

        let mut resealed = 0usize;
        {
            let mut state = self.state.lock().await;
            for key in state.hex.keys() {
                let in_trash = trash && key.tessellation == TRASH_TESSELLATION;
                if !affected.contains(&key.tessellation) && !in_trash {
                    continue;
                }
                let Some(meta) = state.hex.meta(&key).cloned() else { continue };
                if meta.tombstone {
                    continue;
                }
                if let Some(Lookup::Live { bytes, seq, ttl }) = state.hex.read(&key) {
                    // Reading opens what was sealed; seal again under the new policy.
                    let Ok(doc) = parse_document(&bytes) else { continue };
                    let bytes = match serde_json::to_vec(&self.sealer.seal_document(&doc, &paths_of(&doc))) {
                        Ok(bytes) => bytes,
                        Err(_) => continue,
                    };
                    state.hex.put(&key, seq, ttl, &bytes, meta.dirty);
                    resealed += 1;
                }
            }
        }
        for tess in &affected {
            self.doc_cache.drop_tessellation(tess);
        }
        self.changes.reseal(|doc| {
            let in_trash = trash && doc.tessellation == TRASH_TESSELLATION;
            if !affected.contains(&doc.tessellation) && !in_trash {
                return None;
            }
            let doc = crate::sealed::unsealed(doc);
            let paths = paths_of(&doc);
            Some(self.sealer.seal_document(&doc, &paths))
        });
        for tess in &affected {
            self.rebuild_tessellation_indexes(tess).await;
        }
        let sealed: usize = affected.iter().map(|t| policy.paths(t).len()).sum();
        info!("🔒 Sealed fields: {} field(s) in {} tessellation(s); {} document(s) in memory resealed.", sealed, affected.len(), resealed);
    }

    /// True if a document read from this tessellation may be kept in the
    /// parsed-document cache (not when it has sealed fields: parsed, they're open).
    pub(crate) fn cacheable(&self, key: &DocKey) -> bool {
        if key.tessellation == TRASH_TESSELLATION {
            return self.seal_policy.read().unwrap().is_empty();
        }
        self.sealed_paths(&key.tessellation).is_empty()
    }
}

/// A trash entry's sealed paths (`paths` of the tessellation it came from,
/// under `document.`); `None` for any other document.
fn trash_paths(doc: &crate::Document, paths: impl Fn(&str) -> Vec<String>) -> Option<Vec<String>> {
    if doc.tessellation != TRASH_TESSELLATION {
        return None;
    }
    let from = match doc.data.get("from") {
        Some(crate::document::FieldValue::String(from)) => from.clone(),
        _ => return Some(Vec::new()),
    };
    Some(paths(&from).into_iter().map(|p| format!("{}.{}", TRASHED_DOCUMENT, p)).collect())
}

#[cfg(test)]
mod tests {
    use crate::{engine::DocumentQuery, filter::Filter, hex::DocKey, users};
    use serde_json::json;

    const CARD: &str = "4111111111111111";
    const OTHER: &str = "5500005555555559";

    fn clerk(mask: serde_json::Value) -> users::RoleInput {
        users::RoleInput {
            name: Some("clerk".into()),
            description: None,
            permissions: Some(vec!["read".into()]),
            restrictions: Some(serde_json::from_value(json!({ "orders": { "mask": mask } })).unwrap()),
        }
    }

    /// What the hex holds for a document, as text.
    async fn in_memory(engine: &super::HexDBEngine, id: ulid::Ulid) -> String {
        match engine.state.lock().await.hex.read(&DocKey::new("orders", id)) {
            Some(crate::hex::Lookup::Live { bytes, .. }) => String::from_utf8_lossy(&bytes).into_owned(),
            _ => String::new(),
        }
    }

    #[tokio::test]
    async fn masked_fields_are_sealed_in_memory_and_open_for_reads() {
        let (engine, dir) = crate::engine::test_engine().await;
        engine.create_tessellation(users::ROLES_TESSELLATION, "system").unwrap();
        let first = engine.insert_documents("orders", vec![json!({ "card": CARD, "name": "Ada" })], None, None).await.unwrap().value[0].id;
        assert!(in_memory(&engine, first).await.contains(CARD), "not masked yet");

        // A role masks the card: what the hex already holds is resealed.
        users::create_role(&engine, clerk(json!({ "card": { "show_last": 4 } }))).await.unwrap();
        assert!(!in_memory(&engine, first).await.contains(CARD));
        let second = engine.insert_documents("orders", vec![json!({ "card": OTHER, "name": "Bo" })], None, None).await.unwrap().value[0].id;
        let held = in_memory(&engine, second).await;
        assert!(!held.contains(OTHER) && held.contains(crate::sealed::MARK), "{}", held);
        for change in engine.changes.since(0, 100).unwrap() {
            let text = serde_json::to_string(&*change).unwrap();
            assert!(!text.contains(CARD) && !text.contains(OTHER), "the change feed keeps them sealed: {}", text);
        }

        // Reads, filters and sorts see the real values.
        let doc = engine.get_document("orders", &second.to_string()).await.unwrap().unwrap();
        assert_eq!(doc.data_json()["card"], OTHER);
        assert!(engine.doc_cache.get(&DocKey::new("orders", second), 0).is_none());
        let query = DocumentQuery { filter: Filter::parse(&json!({ "card": CARD })).unwrap(), ..Default::default() };
        let page = engine.query_documents("orders", &query).await.unwrap();
        assert_eq!(page.documents.iter().map(|d| d.id).collect::<Vec<_>>(), vec![first]);

        // An index on the card holds tokens and isn't used by the planner, so results don't change.
        engine.create_index("orders", crate::index::IndexDef { name: "card".into(), kind: crate::index::IndexKind::Field, fields: vec!["card".into()], unique: true, analyzer: None, ..Default::default() }).await.unwrap();
        let page = engine.query_documents("orders", &query).await.unwrap();
        assert_eq!(page.documents.len(), 1);
        assert!(page.indexes.is_empty(), "a sealed field's index isn't used");
        let duplicate = engine.insert_documents("orders", vec![json!({ "card": CARD })], None, None).await;
        assert!(duplicate.is_err(), "the unique index still holds");

        // A restart (WAL replay) keeps them sealed.
        engine.shutdown().await.unwrap();
        drop(engine);
        let mut config = crate::HexConfig::default();
        config.storage.path = dir.join("data").to_string_lossy().into_owned();
        config.storage.encryption_key = "base64:BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=".into();
        let keys = std::sync::Arc::new(crate::crypt::KeyRing::new(&[7u8; 32], &[]));
        let identity = crate::engine::HexIdentity { id: ulid::Ulid::generate(), name: "Test".into(), hex_type: crate::network::discovery::ROLE_OVERSEER.into() };
        let engine = super::HexDBEngine::open(config, identity, keys).await.unwrap();
        let doc = engine.get_document("orders", &first.to_string()).await.unwrap().unwrap();
        assert_eq!(doc.data_json()["card"], CARD);
        assert!(!in_memory(&engine, first).await.contains(CARD));

        // Without the mask, sealed values still read, and are written back plain.
        users::update_role(&engine, "clerk", users::RoleInput { restrictions: Some(Default::default()), ..clerk(json!({})) }).await.unwrap();
        assert_eq!(engine.get_document("orders", &first.to_string()).await.unwrap().unwrap().data_json()["card"], CARD);
        assert!(in_memory(&engine, first).await.contains(CARD));
        engine.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
