// HexDB Core Engine: index management
//
// See `crate::index` for what indexes do. This file creates, drops, lists, and
// rebuilds them, and prepares filters for the query paths.

use super::{EngineError, HexDBEngine};
use crate::{
    filter::Filter,
    index::{plan, Index, IndexDef, IndexInfo, Plan, MAX_INDEXES},
};
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
            list.push(Index::new(def.clone()));
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
            self.indexes.write().unwrap().by_tessellation.entry(tess.clone()).or_default().push(Index::new(def.clone()));
            if let Err(e) = self.build_index(&tess, &def.name).await {
                // A unique index can only fail here if data was changed outside HexDB; keep serving without it.
                tracing::error!("❌ Index '{}' on '{}' couldn't be rebuilt and is disabled: {:#}", def.name, tess, e);
                self.remove_index_entry(&tess, &def.name);
                continue;
            }
            let info = self.mark_ready(&tess, &def.name);
            info!("📇 Rebuilt index '{}' on '{}' ({} documents).", def.name, tess, info.documents);
        }
        Ok(())
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
    pub(crate) async fn query_sorted_by_index(
        &self,
        tess: &str,
        query: &crate::engine::DocumentQuery,
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
        let mut seen = HashSet::new();
        let mut documents = Vec::new();
        let mut scanned = 0;
        for (part, id) in ordered {
            if documents.len() >= want {
                break;
            }
            if !seen.insert(id) {
                continue;
            }
            scanned += 1;
            let Some(doc) = self.read_latest(&crate::hex::DocKey::new(tess, id)).await?.0 else { continue };
            let json = doc.to_api_json();
            let value = crate::filter::resolve(&json, &path).first().map(|v| (*v).clone()).unwrap_or(serde_json::Value::Null);
            if crate::index::KeyPart::from_scalar(&value) != Some(part) {
                return Ok(None);
            }
            documents.push(doc);
        }
        let total = self.count_documents(tess).await?;
        let documents = documents.into_iter().skip(query.offset).take(query.limit).collect();
        Ok(Some(crate::engine::QueryPage { documents, total, next: None, indexes: vec![name], scanned }))
    }

    /// Bind `$text` to the tessellation's text index fields, and ask the
    /// planner for candidates. Returns the filter to evaluate and the plan.
    pub(crate) fn prepare_filter(&self, tess: &str, filter: &Filter) -> (Filter, Option<Plan>) {
        let indexes = self.indexes.read().unwrap();
        let filter = match (filter.has_text(), indexes.text_fields(tess)) {
            (true, Some(fields)) => filter.with_text_fields(&fields),
            _ => filter.clone(),
        };
        let plan = indexes.by_tessellation.get(tess).and_then(|list| plan(list, &filter));
        (filter, plan)
    }
}
