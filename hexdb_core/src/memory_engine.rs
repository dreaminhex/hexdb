// HexDB Core Memory Engine Module
// This module implements a memory-based engine for HexDB, allowing for
// in-memory storage and retrieval of documents. The memory engine is designed
// to be fast and efficient. It uses a hash map to store documents, and provides
// methods for inserting, updating, deleting, and retrieving documents.

use crate::{
    document::{infer_fields_from_json, Document}, engine::Engine, hex::HexNode, sst::{SstReader, SstWriter}, wal::Wal
};
use anyhow::{Result, bail, Context};
use chrono::Utc;
use serde_json::Value;
use tracing::{info, warn};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use dashmap::DashMap;
use tokio::{fs, sync::{mpsc::Sender, Mutex}};
use ulid::Ulid;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone)]
pub struct MemoryEngine {
    pub store: Arc<DashMap<String, Document>>,
    pub node: Arc<Mutex<HexNode>>,
    pub tess_map: Arc<DashMap<String, String>>,
    pub total_doc_bytes: Arc<AtomicUsize>,
    pub doc_count: Arc<AtomicUsize>,
    wal_tx: Sender<Wal>,
}

impl MemoryEngine {
    pub fn new(wal_tx: Sender<Wal>) -> Self {
        Self {
            store: Arc::new(DashMap::new()),
            node: Arc::new(Mutex::new(HexNode::new())),
            tess_map: Arc::new(DashMap::new()),
            total_doc_bytes: Arc::new(AtomicUsize::new(0)),
            doc_count: Arc::new(AtomicUsize::new(0)),
            wal_tx,
        }
    }

    fn current_avg_doc_size(&self) -> usize {
        let count = self.doc_count.load(Ordering::Relaxed).max(1);
        self.total_doc_bytes.load(Ordering::Relaxed) / count
    }

    pub fn adaptive_max_docs(&self, ram_mb: u32) -> usize {
        let bytes = ram_mb as usize * 1024 * 1024;
        let avg = self.current_avg_doc_size().max(512); // Minimum floor
        bytes / avg
    }

    fn track_doc_size(&self, doc: &Document) {
        if let Ok(json) = serde_json::to_vec(doc) {
            self.total_doc_bytes.fetch_add(json.len(), Ordering::Relaxed);
            self.doc_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub async fn insert_json(&self, tess: &str, json: Value) -> Result<Document> {
        let id = Ulid::new();
        let data = infer_fields_from_json(&json);

        let doc = Document {
            id,
            tessellation: tess.to_string(),
            data,
            ttl: None,
        };

        info!("📝 Inserting document: {}", doc.id);

        self.track_doc_size(&doc);
        self.tess_map.insert(id.to_string(), tess.to_string());
        self.store.insert(id.to_string(), doc.clone());
        
        if let Err(e) = self.wal_tx.send(Wal::Insert(doc.clone())).await {
            warn!("❗ Failed to send WAL Insert operation: {}", e);
        }

        Ok(doc)
    }

    pub async fn update_json(&self, tess: &str, json: Value) -> Result<Document> {
        let id_str = json
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("❌ Missing 'id' field."))?;

        let data = infer_fields_from_json(&json);

        let mut doc = self
            .store
            .get_mut(id_str)
            .ok_or_else(|| anyhow::anyhow!("❗ Document not found."))?;

        if doc.tessellation != tess {
            bail!("❗ Document exists, but in a different tessellation: {}", doc.tessellation);
        }

        doc.data = data;

        info!("📝 Updating document: {}", doc.id);

        self.track_doc_size(&doc);
        self.tess_map.insert(id_str.to_string(), tess.to_string());
        self.store.insert(id_str.to_string(), doc.clone());

        if let Err(e) = self.wal_tx.send(Wal::Insert(doc.clone())).await {
            warn!("❗ Failed to send WAL Update operation: {}", e);
        }

        Ok(doc.clone())
    }

    pub async fn patch_json(&self, tess: &str, json: Value) -> Result<Document> {
        let id_str = json
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("❌ Missing 'id' field"))?;

        let mut doc = self
            .store
            .get_mut(id_str)
            .ok_or_else(|| anyhow::anyhow!("❗ Document not found"))?;

        if doc.tessellation != tess {
            bail!("❗ Document exists, but in a different tessellation: {}", doc.tessellation);
        }

        let patch_fields = infer_fields_from_json(&json);

        for (k, v) in patch_fields {
            if k != "id" && k != "tessellation" {
                doc.data.insert(k, v);
            }
        }

        self.track_doc_size(&doc);
        self.tess_map.insert(id_str.to_string(), tess.to_string());

        if let Err(e) = self.wal_tx.send(Wal::Insert(doc.clone())).await {
            warn!("❗ Failed to send WAL Update operation: {}", e);
        }

        Ok(doc.clone())
    }

    pub async fn flush_to_sstable(&self) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        let mut tess_map: HashMap<String, Vec<(Ulid, Document)>> = HashMap::new();

        for entry in self.store.iter() {
            let doc = entry.value().clone();
            if let Some(ttl) = doc.ttl {
                if ttl < now {
                    self.store.remove(&entry.key().to_string());
                    self.tess_map.remove(&entry.key().to_string());
                    continue;
                }
            }
            tess_map
                .entry(doc.tessellation.clone())
                .or_default()
                .push((doc.id, doc));
        }

        for (tess, docs) in tess_map.into_iter() {
            if docs.is_empty() {
                continue;
            }

            let dir = PathBuf::from(format!(".hexdb/{}", tess));
            fs::create_dir_all(&dir).await?;

            let filename = format!("{}.hxs", Ulid::new());
            let path = dir.join(filename);

            SstWriter::write(&path, docs.clone())
                .with_context(|| format!("Failed to write SSTable for {}", tess))?;

            for (id, _) in docs {
                self.store.remove(&id.to_string());
                self.tess_map.remove(&id.to_string());
            }
        }

        self.total_doc_bytes.store(0, Ordering::Relaxed);
        self.doc_count.store(0, Ordering::Relaxed);
        info!("✅ Flushed documents to SSTable successfully.");
        Ok(())
    }
}

#[async_trait]
impl Engine for MemoryEngine {
    async fn insert_document(&self, doc: Document) -> Result<()> {
        let id = doc.id.to_string();
        self.tess_map.insert(id.clone(), doc.tessellation.clone());
        self.store.insert(id, doc.clone());

        if let Err(e) = self.wal_tx.send(Wal::Insert(doc)).await {
            warn!("❗ Failed to send WAL Insert operation: {}", e);
        }

        Ok(())
    }

    async fn get_document(&self, tess: &str, id: &str) -> Result<Option<Document>> {
        if let Some(doc) = self.store.get(id) {
            if doc.tessellation == tess {
                return Ok(Some(doc.clone()));
            }
        }

        let tess_from_map = match self.tess_map.get(id) {
            Some(t) => t.value().clone(),
            None => return Ok(None),
        };

        if tess_from_map != tess {
            return Ok(None);
        }

        let dir = PathBuf::from(format!(".hexdb/{}", tess));
        if !dir.exists() {
            return Ok(None);
        }

        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().map_or(false, |ext| ext == "hxs") {
                let map = SstReader::load_all(&path)?;
                if let Some(doc) = map.get(&Ulid::from_string(id)?) {
                    return Ok(Some(doc.clone()));
                }
            }
        }

        Ok(None)
    }

    async fn delete_document(&self, tess: &str, id: &str) -> Result<()> {
        if let Some(doc) = self.store.get(id) {
            if doc.tessellation != tess {
                bail!("❗ Document exists, but in a different tessellation: {}", doc.tessellation);
            }
        }

        self.store.remove(id);
        self.tess_map.remove(id);

        if let Err(e) = self.wal_tx.send(Wal::Delete {
            tessellation: tess.to_string(),
            id: id.to_string(),
        }).await {
            warn!("❗ Failed to send WAL Delete operation: {}", e);
        }

        Ok(())
    }

    async fn count_documents(&self, tess: &str) -> Result<usize> {
        Ok(self
            .store
            .iter()
            .filter(|doc| doc.tessellation == tess)
            .count())
    }

    async fn insert_json(&self, tess: &str, json: Value) -> Result<Document> {
        Self::insert_json(self, tess, json).await
    }

    async fn update_json(&self, tess: &str, json: Value) -> Result<Document> {
        Self::update_json(self, tess, json).await
    }

    async fn patch_json(&self, tess: &str, json: Value) -> Result<Document> {
        Self::patch_json(self, tess, json).await
    }
}
