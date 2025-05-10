use crate::document::Document;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

#[async_trait]
pub trait Engine: Send + Sync {
    // Insert a fully-formed document with known ID and type-safe data
    async fn insert_document(&self, doc: Document) -> Result<()>;

    // Retrieve a document by ID from a specific tessellation
    async fn get_document(&self, tessellation: &str, id: &str) -> Result<Option<Document>>;

    // Delete a document by ID from a specific tessellation
    async fn delete_document(&self, tessellation: &str, id: &str) -> Result<()>;

    // Return number of documents in a specific tessellation
    async fn count_documents(&self, tessellation: &str) -> Result<usize>;

    // Insert from raw JSON, inferring types and generating ULID
    async fn insert_json(&self, tessellation: &str, json: Value) -> Result<Document>;

    // Replace a document fully using raw JSON
    async fn update_json(&self, tessellation: &str, json: Value) -> Result<Document>;

    // Patch a document (partial update) using raw JSON
    async fn patch_json(&self, tessellation: &str, json: Value) -> Result<Document>;
}
