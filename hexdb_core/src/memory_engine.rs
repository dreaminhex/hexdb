use std::sync::Arc;
use dashmap::DashMap;
use crate::{document::Document, engine::Engine, hex::HexNode};
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct MemoryEngine {
    store: Arc<DashMap<String, Document>>,
    pub node: Arc<Mutex<HexNode>>, // Added HexNode as internal engine
}

impl MemoryEngine {
    pub fn new() -> Self {
        Self {
            store: Arc::new(DashMap::new()),
            node: Arc::new(Mutex::new(HexNode::new())),
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
