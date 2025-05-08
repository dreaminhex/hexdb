use crate::document::Document;
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait Engine: Send + Sync {
    /// Insert or update a document by ID
    async fn upsert(&self, doc: Document) -> Result<()>;

    /// Retrieve a document by ID
    async fn get(&self, id: &str) -> Result<Option<Document>>;

    /// Delete a document by ID
    async fn delete(&self, id: &str) -> Result<()>;

    /// Return number of documents
    async fn count(&self) -> Result<usize>;
}
