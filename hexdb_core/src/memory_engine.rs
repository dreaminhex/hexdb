// HexDB Core Memory Engine Module
// This module implements a memory-based engine for HexDB, allowing for
// in-memory storage and retrieval of documents. The memory engine is designed
// to be fast and efficient. It uses a hash map to store documents, and provides
// methods for inserting, updating, deleting, and retrieving documents.

use crate::{
    document::{Document, infer_fields_from_json},
    hex::HexNode,
    engine::Engine,
    wal::Wal,
};
use anyhow::{Result, bail};
use serde_json::Value;
use tracing::{info, warn};
use std::sync::Arc;
use dashmap::DashMap;
use tokio::{sync::Mutex, sync::mpsc::Sender};
use ulid::Ulid;
use async_trait::async_trait;

#[derive(Clone)]
pub struct MemoryEngine {
    pub store: Arc<DashMap<String, Document>>,
    pub node: Arc<Mutex<HexNode>>,
    wal_tx: Sender<Wal>,
}

impl MemoryEngine {
    pub fn new(wal_tx: Sender<Wal>) -> Self {
        Self {
            store: Arc::new(DashMap::new()),
            node: Arc::new(Mutex::new(HexNode::new())),
            wal_tx,
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

        let id = Ulid::from_string(id_str)?;
        let data = infer_fields_from_json(&json);

        let mut doc = self
            .store
            .get_mut(id_str)
            .ok_or_else(|| anyhow::anyhow!("❗ Document not found."))?;

        if doc.tessellation != tess {
            bail!("❗ Document exists, but in a different tessellation: {}", doc.tessellation);
        }

        doc.data = data;
        // TODO: Update/Handle TTL if needed

        info!("📝 Updating document: {}", doc.id);

        self.store.insert(id.to_string(), doc.clone());

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

        if let Err(e) = self.wal_tx.send(Wal::Insert(doc.clone())).await {
            warn!("❗ Failed to send WAL Update operation: {}", e);
        }

        Ok(doc.clone())
    }
}

#[async_trait]
impl Engine for MemoryEngine {
    async fn insert_document(&self, doc: Document) -> Result<()> {
        let id = doc.id.to_string();

        let store_doc = doc.clone();
        self.store.insert(id, store_doc);

        if let Err(e) = self.wal_tx.send(Wal::Insert(doc)).await {
            warn!("❗ Failed to send WAL Insert operation: {}", e);
        }

        Ok(())
    }

    async fn get_document(&self, tess: &str, id: &str) -> Result<Option<Document>> {
        Ok(self.store.get(id).and_then(|doc| {
            if doc.tessellation == tess {
                Some(doc.clone())
            } else {
                None
            }
        }))
    }

    async fn delete_document(&self, tess: &str, id: &str) -> Result<()> {
        if let Some(doc) = self.store.get(id) {
            if doc.tessellation != tess {
                bail!("❗ Document exists, but in a different tessellation: {}", doc.tessellation);
            }
        }
        self.store.remove(id);

        // Send delete operation to WAL to persist as a tombstone entry
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
