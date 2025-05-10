use crate::{
    document::{Document, infer_fields_from_json},
    hex::HexNode,
    engine::Engine,
};
use anyhow::{Result, bail};
use serde_json::Value;
use std::sync::Arc;
use dashmap::DashMap;
use tokio::sync::Mutex;
use ulid::Ulid;
use async_trait::async_trait;

#[derive(Clone)]
pub struct MemoryEngine {
    store: Arc<DashMap<String, Document>>,
    pub node: Arc<Mutex<HexNode>>,
}

impl MemoryEngine {
    pub fn new() -> Self {
        Self {
            store: Arc::new(DashMap::new()),
            node: Arc::new(Mutex::new(HexNode::new())),
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

        self.store.insert(id.to_string(), doc.clone());
        Ok(doc)
    }

    pub async fn update_json(&self, tess: &str, json: Value) -> Result<Document> {
        let id_str = json
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("❌ Missing 'id' field"))?;

        let id = Ulid::from_string(id_str)?;
        let data = infer_fields_from_json(&json);

        let doc = Document {
            id,
            tessellation: tess.to_string(),
            data,
            ttl: None,
        };

        self.store.insert(id.to_string(), doc.clone());
        Ok(doc)
    }

    pub async fn patch_json(&self, tess: &str, json: Value) -> Result<Document> {
        let id_str = json
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("❌ Missing 'id' field"))?;

        let mut doc = self
            .store
            .get_mut(id_str)
            .ok_or_else(|| anyhow::anyhow!("⚠️ Document not found"))?;

        if doc.tessellation != tess {
            bail!("⚠️ Document exists, but in a different tessellation: {}", doc.tessellation);
        }

        let patch_fields = infer_fields_from_json(&json);

        for (k, v) in patch_fields {
            if k != "id" && k != "tessellation" {
                doc.data.insert(k, v);
            }
        }

        Ok(doc.clone())
    }
}

#[async_trait]
impl Engine for MemoryEngine {
    async fn insert_document(&self, doc: Document) -> Result<()> {
        let id = doc.id.to_string();
        self.store.insert(id, doc);
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
                bail!("⚠️ Document exists, but in a different tessellation: {}", doc.tessellation);
            }
        }
        self.store.remove(id);
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
