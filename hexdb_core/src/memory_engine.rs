use std::sync::Arc;
use dashmap::DashMap;
use crate::{document::Document, engine::Engine};
use anyhow::Result;
use async_trait::async_trait;

#[derive(Clone)]
pub struct MemoryEngine {
    store: Arc<DashMap<String, Document>>,
}

impl MemoryEngine {
    pub fn new() -> Self {
        Self {
            store: Arc::new(DashMap::new()),
        }
    }
}

#[async_trait]
impl Engine for MemoryEngine {
    async fn upsert(&self, doc: Document) -> Result<()> {
        self.store.insert(doc.id.clone(), doc);
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<Document>> {
        Ok(self.store.get(id).map(|v| v.clone()))
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.store.remove(id);
        Ok(())
    }

    async fn count(&self) -> Result<usize> {
        Ok(self.store.len())
    }
}
