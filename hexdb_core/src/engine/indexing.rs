// HexDB Core Engine: index management
//
// See `crate::index` for what indexes do. This file creates, drops, lists, and
// rebuilds them, and prepares filters for the query paths.

use super::{EngineError, HexDBEngine};
use crate::{
    filter::Filter,
    index::{plan, Index, IndexDef, IndexInfo, IndexSnapshot, Plan, MAX_INDEXES, SNAPSHOT_VERSION},
};

/// Folder of index snapshots in the data directory.
const SNAPSHOT_DIR: &str = "indexes";
use anyhow::Result;
use std::collections::HashSet;
use tracing::info;

fn invalid(message: impl Into<String>) -> anyhow::Error {
    EngineError::Invalid(message.into()).into()
}

impl HexDBEngine {
    /// Indexes of a tessellation.
    pub fn list_indexes(&self, tess: &str) -> Vec<IndexInfo> {
        let indexes = self.indexes.read().unwrap();
        indexes.by_tessellation.get(tess).map(|list| list.iter().map(Index::info).collect()).unwrap_or_default()
    }

    /// Create an index and build it from the existing documents. Fails (and
    /// leaves no index behind) if a unique index finds duplicates.
    pub async fn create_index(&self, tess: &str, def: IndexDef) -> Result<IndexInfo> {
        self.ensure_writable()?;
        self.create_index_unchecked(tess, def).await
    }

    /// Create an index without the write guard (replication).
    pub(crate) async fn create_index_unchecked(&self, tess: &str, def: IndexDef) -> Result<IndexInfo> {
        if self.is_system_tessellation(tess) {
            return Err(invalid(format!("'{}' is a system tessellation; it can't be indexed.", tess)));
        }
        self.create_index_internal(tess, def).await
    }

    /// Create an index HexDB itself relies on, on any tessellation, if it
    /// doesn't exist yet (e.g. the users' login index).
    pub(crate) async fn ensure_internal_index(&self, tess: &str, def: IndexDef) -> Result<()> {
        let exists = self.list_indexes(tess).iter().any(|i| i.def.name == def.name);
        if !exists {
            self.create_index_internal(tess, def).await?;
        }
        Ok(())
    }

    async fn create_index_internal(&self, tess: &str, def: IndexDef) -> Result<IndexInfo> {
        let def = def.validated()?;
        let analyzer = self.analyzer_for(&def)?;
        if !self.tessellation_exists(tess) {
            return Err(EngineError::NotFound(format!("Tessellation '{}' not found.", tess)).into());
        }
        {
            let mut indexes = self.indexes.write().unwrap();
            let list = indexes.by_tessellation.entry(tess.to_string()).or_default();
            if list.iter().any(|i| i.def.name == def.name) {
                return Err(EngineError::Conflict(format!("'{}' already has an index named '{}'.", tess, def.name)).into());
            }
            if list.iter().any(|i| i.def.kind == def.kind && i.def.fields == def.fields && def.kind != crate::index::IndexKind::Text) {
                return Err(EngineError::Conflict(format!("'{}' already has an index on {:?}.", tess, def.fields)).into());
            }
            if def.kind == crate::index::IndexKind::Text && list.iter().any(|i| i.def.kind == crate::index::IndexKind::Text) {
                return Err(EngineError::Conflict(format!(
                    "'{}' already has a text index; a tessellation can have one (it can cover several fields).",
                    tess
                ))
                .into());
            }
            if list.len() >= MAX_INDEXES {
                return Err(invalid(format!("A tessellation can have at most {} indexes.", MAX_INDEXES)));
            }
            // Registered before the scan so concurrent writes keep it current.
            list.push(Index::with_analyzer(def.clone(), analyzer));
        }

        let built = self.build_index(tess, &def.name).await;
        if let Err(e) = built {
            self.remove_index_entry(tess, &def.name);
            return Err(e);
        }
        {
            let mut catalog = self.catalog.lock().unwrap();
            if let Some(info) = catalog.tessellations.get_mut(tess) {
                info.indexes.retain(|d| d.name != def.name);
                info.indexes.push(def.clone());
            }
            if let Err(e) = catalog.save(&self.storage_dir, &self.keys) {
                drop(catalog);
                self.remove_index_entry(tess, &def.name);
                return Err(e);
            }
        }
        let info = self.mark_ready(tess, &def.name);
        info!("📇 Created index '{}' on '{}' ({:?}, {} documents).", def.name, tess, def.fields, info.documents);
        Ok(info)
    }

    /// Drop an index. Returns false if it didn't exist.
    pub fn drop_index(&self, tess: &str, name: &str) -> Result<bool> {
        self.ensure_writable()?;
        self.drop_index_unchecked(tess, name)
    }

    /// Drop an index without the write guard (replication).
    pub(crate) fn drop_index_unchecked(&self, tess: &str, name: &str) -> Result<bool> {
        if !self.remove_index_entry(tess, name) {
            return Ok(false);
        }
        let mut catalog = self.catalog.lock().unwrap();
        if let Some(info) = catalog.tessellations.get_mut(tess) {
            info.indexes.retain(|d| d.name != name);
        }
        catalog.save(&self.storage_dir, &self.keys)?;
        info!("🗑️ Dropped index '{}' on '{}'.", name, tess);
        Ok(true)
    }

    /// Build every index in the catalog (at startup).
    pub(crate) async fn rebuild_indexes(&self) -> Result<()> {
        let defs: Vec<(String, IndexDef)> = {
            let catalog = self.catalog.lock().unwrap();
            catalog
                .tessellations
                .iter()
                .flat_map(|(tess, info)| info.indexes.iter().map(move |d| (tess.clone(), d.clone())))
                .collect()
        };
        for (tess, def) in defs {
            if self.restore_index(&tess, &def).await {
                continue;
            }
            let analyzer = match self.analyzer_for(&def) {
                Ok(analyzer) => analyzer,
                Err(e) => {
                    tracing::error!("❌ Index '{}' on '{}' is disabled: {:#}", def.name, tess, e);
                    continue;
                }
            };
            self.indexes.write().unwrap().by_tessellation.entry(tess.clone()).or_default().push(Index::with_analyzer(def.clone(), analyzer));
            if let Err(e) = self.build_index(&tess, &def.name).await {
                // A unique index can only fail here if data was changed outside HexDB; keep serving without it.
                tracing::error!("❌ Index '{}' on '{}' couldn't be rebuilt and is disabled: {:#}", def.name, tess, e);
                self.remove_index_entry(&tess, &def.name);
                continue;
            }
            let info = self.mark_ready(&tess, &def.name);
            info!("📇 Rebuilt index '{}' on '{}' ({} documents).", def.name, tess, info.documents);
        }
        self.indexes_loaded.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// The analyzer a text index uses (standard for field indexes).
    pub(crate) fn analyzer_for(&self, def: &IndexDef) -> Result<crate::analysis::Analyzer> {
        crate::analysis::find(def.analyzer.as_deref(), &self.config.analyzers).map_err(|e| invalid(e.to_string()))
    }

    /// The snapshot file of an index.
    fn snapshot_path(&self, tess: &str, name: &str) -> std::path::PathBuf {
        self.storage_dir.join(SNAPSHOT_DIR).join(tess).join(format!("{}.hxi", name))
    }

    /// Load an index from its snapshot and apply what changed since. False if
    /// there is no usable snapshot (the caller rebuilds the index).
    async fn restore_index(&self, tess: &str, def: &IndexDef) -> bool {
        let path = self.snapshot_path(tess, &def.name);
        let keys = self.keys.clone();
        let loaded = tokio::task::spawn_blocking(move || -> Result<Option<IndexSnapshot>> {
            let Some(bytes) = crate::crypt::read_sealed_file(&path, &keys)? else { return Ok(None) };
            let json = crate::compress::decompress(&bytes[..])?;
            Ok(Some(serde_json::from_slice(&json)?))
        })
        .await;
        let snapshot = match loaded {
            Ok(Ok(Some(snapshot))) => snapshot,
            Ok(Ok(None)) => return false,
            Ok(Err(e)) => {
                tracing::warn!("⚠️ The snapshot of index '{}' on '{}' is unreadable ({:#}); rebuilding it.", def.name, tess, e);
                return false;
            }
            Err(_) => return false,
        };
        let dropped_after = self.catalog.lock().unwrap().dropped.get(tess).is_some_and(|d| *d > snapshot.seq);
        let max_seq = self.stats().await.next_seq.saturating_sub(1);
        if snapshot.version != SNAPSHOT_VERSION || snapshot.tessellation != tess || snapshot.def != *def || dropped_after || snapshot.seq > max_seq {
            return false;
        }
        let Ok(analyzer) = self.analyzer_for(def) else { return false };
        if def.kind == crate::index::IndexKind::Text && snapshot.analysis != analyzer.pipeline() {
            // The analyzer changed since the snapshot: rebuild with the new one.
            return false;
        }
        let seq = snapshot.seq;
        self.indexes.write().unwrap().by_tessellation.entry(tess.to_string()).or_default().push(Index::from_snapshot(snapshot, analyzer));
        // Bring it up to date with every document written after the snapshot.
        let changed = self.sst.ids_changed_after(tess, seq).await;
        let mut applied = 0usize;
        for id in &changed {
            let doc = match self.read_latest(&crate::hex::DocKey::new(tess, *id)).await {
                Ok((doc, _)) => doc,
                Err(e) => {
                    tracing::warn!("⚠️ Couldn't read {} while restoring index '{}' on '{}' ({:#}); rebuilding it.", id, def.name, tess, e);
                    self.remove_index_entry(tess, &def.name);
                    return false;
                }
            };
            let mut indexes = self.indexes.write().unwrap();
            if let Some(index) = indexes.by_tessellation.get_mut(tess).and_then(|l| l.iter_mut().find(|i| i.def.name == def.name)) {
                match &doc {
                    Some(doc) => index.insert(doc),
                    None => index.remove(id),
                }
                applied += 1;
            }
        }
        let info = self.mark_ready(tess, &def.name);
        info!("📇 Loaded index '{}' on '{}' ({} documents; {} updated since its snapshot).", def.name, tess, info.documents, applied);
        true
    }

    /// Save every index's contents (after a flush, and at shutdown). The
    /// snapshot reflects exactly the writes up to a sequence number: commits
    /// are paused while it is copied, and only once everything before that
    /// number is durable, so a snapshot never holds a write that a crash
    /// could still lose.
    pub(crate) async fn save_index_snapshots(&self) {
        // Before startup has loaded the indexes there is nothing to save, and
        // the snapshots on disk must not be cleaned up.
        if !self.indexes_loaded.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let snapshots: Vec<IndexSnapshot> = {
            let state = self.state.lock().await;
            let seq = state.next_seq.saturating_sub(1);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while self.changes.published_seq() < seq {
                if std::time::Instant::now() > deadline {
                    tracing::debug!("Skipping index snapshots: writes are still being made durable.");
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            let indexes = self.indexes.read().unwrap();
            indexes
                .by_tessellation
                .iter()
                .flat_map(|(tess, list)| list.iter().filter(|i| i.ready).map(move |i| i.snapshot(tess, seq)))
                .collect()
        };
        let base = self.storage_dir.join(SNAPSHOT_DIR);
        let keys = self.keys.clone();
        let wanted: Vec<(String, String)> = snapshots.iter().map(|s| (s.tessellation.clone(), s.def.name.clone())).collect();
        let result = tokio::task::spawn_blocking(move || -> Result<usize> {
            for snapshot in &snapshots {
                let dir = base.join(&snapshot.tessellation);
                std::fs::create_dir_all(&dir)?;
                let json = serde_json::to_vec(snapshot)?;
                let compressed = crate::compress::compress(&json[..], 1)?;
                crate::crypt::write_sealed_file(&dir.join(format!("{}.hxi", snapshot.def.name)), &keys, &compressed)?;
            }
            // Remove snapshots of indexes (and tessellations) that no longer exist.
            let mut removed = 0;
            if let Ok(dirs) = std::fs::read_dir(&base) {
                for dir in dirs.flatten() {
                    let tess = dir.file_name().to_string_lossy().into_owned();
                    for file in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
                        let name = file.path().file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        if !wanted.iter().any(|(t, n)| *t == tess && *n == name) {
                            let _ = std::fs::remove_file(file.path());
                            removed += 1;
                        }
                    }
                    let _ = std::fs::remove_dir(dir.path());
                }
            }
            Ok(removed)
        })
        .await;
        match result {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!("⚠️ Couldn't save index snapshots: {:#}", e),
            Err(e) => tracing::warn!("⚠️ Couldn't save index snapshots: {}", e),
        }
    }

    /// Add every existing document to a registered index.
    async fn build_index(&self, tess: &str, name: &str) -> Result<()> {
        let documents = self.matching_documents_scan(tess, &Filter::all()).await?;
        let mut indexes = self.indexes.write().unwrap();
        let Some(index) = indexes.by_tessellation.get_mut(tess).and_then(|l| l.iter_mut().find(|i| i.def.name == name)) else {
            return Ok(());
        };
        for doc in &documents {
            // A write during the scan already indexed the newer version.
            if index.contains(&doc.id) {
                continue;
            }
            if let Some(other) = index.unique_conflict(doc, &HashSet::new()) {
                return Err(EngineError::Conflict(format!(
                    "Can't create unique index '{}': documents {} and {} share a value.",
                    name, other, doc.id
                ))
                .into());
            }
            index.insert(doc);
        }
        Ok(())
    }

    fn mark_ready(&self, tess: &str, name: &str) -> IndexInfo {
        let mut indexes = self.indexes.write().unwrap();
        let index = indexes
            .by_tessellation
            .get_mut(tess)
            .and_then(|l| l.iter_mut().find(|i| i.def.name == name))
            .expect("index registered");
        index.ready = true;
        index.info()
    }

    fn remove_index_entry(&self, tess: &str, name: &str) -> bool {
        let mut indexes = self.indexes.write().unwrap();
        let Some(list) = indexes.by_tessellation.get_mut(tess) else { return false };
        let before = list.len();
        list.retain(|i| i.def.name != name);
        before != list.len()
    }

    /// An unfiltered query sorted by one field that has a single-field index:
    /// read documents in index order and stop once the page is full. Returns
    /// `None` (use the general path) when there's no such index, or when a
    /// document's sort value doesn't match its index key (arrays, objects),
    /// so results are always the same as an in-memory sort.
    /// Answer a sorted query by walking the first sort key's field index in
    /// order: candidates outside the plan are skipped without being read, the
    /// filter is checked on the rest, and reading stops once the page (plus
    /// any documents tied with its last one on the first key, which the other
    /// sort keys order) is complete. `None` when no suitable index exists or
    /// the index disagrees with a document (the caller falls back to sorting
    /// in memory). Counts the total only for unfiltered queries.
    pub(crate) async fn query_sorted_by_index(
        &self,
        tess: &str,
        query: &crate::engine::DocumentQuery,
        filter: &crate::filter::Filter,
        plan: Option<&Plan>,
    ) -> Result<Option<crate::engine::QueryPage>> {
        let key = &query.sort[0];
        let (name, ordered) = {
            let indexes = self.indexes.read().unwrap();
            let Some(index) = indexes.by_tessellation.get(tess).and_then(|list| {
                list.iter().find(|i| i.ready && i.def.kind == crate::index::IndexKind::Field && i.def.fields == [key.field.clone()])
            }) else {
                return Ok(None);
            };
            (index.def.name.clone(), index.ordered(key.descending))
        };
        let Some(ordered) = ordered else { return Ok(None) };
        let path: Vec<String> = key.field.split('.').map(String::from).collect();
        let want = query.offset.saturating_add(query.limit);
        let ties_matter = query.sort.len() > 1;
        let mut seen = HashSet::new();
        let mut documents = Vec::new();
        let mut last_part = None;
        let mut scanned = 0;
        for (part, id) in ordered {
            if documents.len() >= want && (!ties_matter || last_part.as_ref() != Some(&part)) {
                break;
            }
            if !seen.insert(id) || plan.is_some_and(|p| !p.candidates.contains(&id)) {
                continue;
            }
            scanned += 1;
            let Some(doc) = self.read_latest(&crate::hex::DocKey::new(tess, id)).await?.0 else { continue };
            let json = doc.to_api_json();
            let value = crate::filter::resolve(&json, &path).first().map(|v| (*v).clone()).unwrap_or(serde_json::Value::Null);
            if crate::index::KeyPart::from_scalar(&value).as_ref() != Some(&part) {
                return Ok(None);
            }
            if !filter.matches(&doc) {
                continue;
            }
            documents.push(doc);
            last_part = Some(part);
        }
        // Documents tied on the first key are ordered by the remaining keys.
        crate::filter::sort_documents(&mut documents, &query.sort);
        let total = if query.with_total && query.filter.is_empty() { Some(self.count_documents(tess).await?) } else { None };
        let mut indexes = plan.map(|p| p.indexes.clone()).unwrap_or_default();
        if !indexes.contains(&name) {
            indexes.push(name);
        }
        let documents = documents.into_iter().skip(query.offset).take(query.limit).collect();
        Ok(Some(crate::engine::QueryPage { documents, total, next: None, indexes, scanned }))
    }

    /// Bind `$text` to the tessellation's text index fields, and ask the
    /// planner for candidates. Returns the filter to evaluate and the plan.
    pub(crate) fn prepare_filter(&self, tess: &str, filter: &Filter) -> (Filter, Option<Plan>) {
        let indexes = self.indexes.read().unwrap();
        let filter = match (filter.has_text(), indexes.text_index(tess)) {
            (true, Some((fields, analyzer))) => filter.with_text_index(&fields, &analyzer),
            _ => filter.clone(),
        };
        let plan = indexes.by_tessellation.get(tess).and_then(|list| plan(list, &filter));
        (filter, plan)
    }
}
