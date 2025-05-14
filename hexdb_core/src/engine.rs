// HexDB Core Engine Module
// This module implements a memory-based engine for HexDB, allowing for
// in-memory storage and retrieval of documents. The memory engine is designed
// to be fast and efficient. It uses a hash map to store documents, and provides
// methods for inserting, updating, deleting, and retrieving documents.

use crate::{
    document::{infer_fields_from_json, Document},
    hex::Hex,
    sst::{SstUtil},
    wal::Wal,
    HexConfig,
};
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use rand::seq::IndexedRandom;
use serde_json::Value;
use std::{
    path::{Path},
    sync::Arc,
};
use tokio::{
    fs::{File},
    io::{self, AsyncBufReadExt, BufReader},
    sync::{mpsc::Sender, Mutex},
};
use tracing::{debug, error, info, warn};
use ulid::Ulid;

#[derive(Clone)]
pub struct HexDBEngine {
    pub node: Arc<Mutex<Hex>>,
    pub config: HexConfig,
    pub wal_tx: Sender<Wal>,
    pub id: Ulid,
    pub name: String,
    pub hex_type: String,
    pub version: String,
    pub start_datetime: DateTime<Utc>,
    pub sst: SstUtil,
}

impl HexDBEngine {
    /// Creates a new instance of the HexDB engine.
    /// It initializes the engine with a new hexagonal structure, loads names from a file,
    /// and picks a random name for the hex.
    /// The engine is designed to be memory-based, allowing for fast and efficient storage and retrieval of documents.
    pub async fn new(wal_tx: Sender<Wal>, config: HexConfig) -> Self {
        let names = Self::load_names("./data/names.txt")
            .await
            .unwrap_or_default();
        let name = Self::pick_random_name(&names).unwrap_or_else(|| "Unnamed Hex".to_string());

        Self {
            node: Arc::new(Mutex::new(Hex::new())),
            config: config.clone(),
            wal_tx,
            id: Ulid::new(),
            name,
            hex_type: "manager".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            start_datetime: Utc::now(),
            sst: SstUtil::new(config),
        }
    }

    /// Loads names from a file into a vector of strings.
    pub async fn load_names<P: AsRef<Path>>(path: P) -> io::Result<Vec<String>> {
        let file = File::open(path).await?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut names = Vec::new();

        while let Some(line) = lines.next_line().await? {
            names.push(line);
        }

        Ok(names)
    }

    /// Picks a random name for the Hex from the provided list of names.
    pub fn pick_random_name(names: &[String]) -> Option<String> {
        let mut rng = rand::rng();
        names.choose(&mut rng).cloned()
    }

    /// Returns the maximum number of documents that can be stored in memory.
    /// This is calculated based on the total RAM available, the average size of documents,
    /// and the number of documents currently stored on disk.
    /// The function also logs the memory status, including the amount of RAM used,
    /// the total RAM available, the average size of documents, and the estimated capacity.
    pub fn adaptive_max_docs(&self) -> usize {
        let ram_bytes = self.node.blocking_lock().calculate_total_ram_bytes();
        let disk_docs = self.sst.count_documents_on_disk().unwrap_or(0);
        let ram_mb = self.config.memory.ram_mb;
        let ram_total = ram_mb as usize * 1024 * 1024;
        let doc_count = self.node.blocking_lock().count_total_documents();
        let avg = if doc_count > 0 { ram_bytes / doc_count } else { 0 };

        let max_docs = if avg > 0 { ram_total / avg } else { usize::MAX };

        let ram_pct = if ram_total > 0 {
            (ram_bytes as f64 / ram_total as f64) * 100.0
        } else {
            0.0
        };

        debug!(
            ram_mb,
            ram_total,
            ram_bytes,
            ram_pct,
            avg,
            disk_docs,
            max_docs,
            "🧠 Memory status: {:.2} KB used / {} MB total ({:.2}%). Estimated capacity: {} docs (~{} MB).",
            ram_bytes as f64 / 1024.0,
            ram_mb,
            ram_pct,
            max_docs,
            max_docs * avg / 1024 / 1024
        );

        max_docs
    }

    /// Reviews all documents in memory and checks if they are expired.
    /// If a document is expired, it will be removed from memory.
    pub async fn sweep_expired_documents(&self) -> Result<usize> {
        let mut removed = 0;
        let now = Utc::now().timestamp_millis();

        let mut node = self.node.lock().await;
        for tess in node.tessellations.keys().cloned().collect::<Vec<_>>() {
            let docs = node.get_all_docs(&tess);
            for (id, raw) in docs {
                let doc: Document = match serde_json::from_slice(&raw) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                if let Some(ttl) = doc.ttl {
                    if ttl < now {
                        node.delete_document(&tess, &id);
                        removed += 1;
                    }
                }
            }
        }

        self.sst.compact().await?;

        if removed > 0 {
            info!("🧹 TTL sweep removed {} expired docs from memory.", removed);
        }

        Ok(removed)
    }

    /// Inserts a JSON document into the specified tessellation, storing the data within the vertices of the hex.
    /// If the tessellation does not exist, it will be created.
    /// Returns the inserted document.
    pub async fn insert_json(&self, tess: &str, json: Value) -> Result<Document> {
        let id = Ulid::new();
        let data = infer_fields_from_json(&json);

        let doc = Document {
            id,
            tessellation: tess.to_string(),
            data,
            ttl: None,
        };

        // Get the hex node, and lock it for writing.
        // This is a mutex lock, so it will block until the lock is available.
        let mut hex = self.node.lock().await;

        // Check if the tessellation exists, if not, create it
        if !hex.tessellation_exists(tess) {
            info!("🧩 Creating new tessellation: {}.", tess);
            hex.create_tessellation(tess, "user");
        }

        // Write to the hex node's vertices
        let data = serde_json::to_vec(&doc)?;

        debug!(
            "📝 Inserting document: {} into tessellation {}.",
            doc.id, tess
        );

        hex.create_document(&doc.tessellation, &doc.id.to_string(), &data);
        drop(hex);

        self.sst.track_document_size(&doc);

        if let Err(e) = self.wal_tx.send(Wal::Insert(doc.clone())).await {
            error!("❗ Failed to send WAL Insert operation: {}", e);
        }

        info!(
            "✅ Document {} inserted into tessellation {}.",
            doc.id, tess
        );

        Ok(doc)
    }

    /// Updates a JSON document in the specified tessellation, storing the data within the vertices of the hex.
    pub async fn update_json(&self, tess: &str, json: Value) -> Result<Document> {
        // Check if the document exists in memory
        let id_str = json
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("❌ Missing 'id' field."))?;

        let mut hex = self.node.lock().await;
        let raw = hex
            .read_document(tess, id_str)
            .ok_or_else(|| anyhow::anyhow!("❗ Document not found."))?;
        let mut doc: Document = serde_json::from_slice(&raw)?;

        if doc.tessellation != tess {
            bail!(
                "❗ Document exists, but in different tessellation: {}",
                doc.tessellation
            );
        }

        doc.data = infer_fields_from_json(&json);

        debug!("📝 Updating document: {}...", doc.id);

        self.sst.track_document_size(&doc);

        let raw = serde_json::to_vec(&doc)?;
        hex.create_document(tess, id_str, &raw);
        drop(hex);

        if let Err(e) = self.wal_tx.send(Wal::Insert(doc.clone())).await {
            error!("❗ Failed to send WAL Update operation: {}", e);
        }

        info!("✅ Document {} updated in tessellation {}.", doc.id, tess);

        Ok(doc)
    }

    /// Patches a JSON document in the specified tessellation, storing the data within the vertices of the hex.
    /// This is a partial update, meaning only the fields provided in the JSON will be updated.
    pub async fn patch_json(&self, tess: &str, json: Value) -> Result<Document> {
        let id_str = json
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("❌ Missing 'id' field"))?;

        let mut hex = self.node.lock().await;
        let raw = hex
            .read_document(tess, id_str)
            .ok_or_else(|| anyhow::anyhow!("❗ Document not found"))?;
        let mut doc: Document = serde_json::from_slice(&raw)?;

        if doc.tessellation != tess {
            bail!(
                "❗ Document exists, but in different tessellation: {}",
                doc.tessellation
            );
        }

        let patch = infer_fields_from_json(&json);

        for (k, v) in patch {
            if k != "id" && k != "tessellation" {
                doc.data.insert(k, v);
            }
        }

        let raw = serde_json::to_vec(&doc)?;
        hex.create_document(tess, id_str, &raw);
        drop(hex);

        self.sst.track_document_size(&doc);

        debug!(
            "📝 Patched document: {} in tessellation {}.",
            doc.id, doc.tessellation
        );

        if let Err(e) = self.wal_tx.send(Wal::Insert(doc.clone())).await {
            error!("❗ Failed to send WAL Update operation: {}", e);
        }

        info!("✅ Document {} patched in tessellation {}.", doc.id, tess);

        Ok(doc)
    }

    /// Method to count the number of documents in a tessellation.
    /// This method retrieves all documents from the specified tessellation
    /// and returns the count.
    pub async fn count_documents(&self, tess: &str) -> Result<usize> {
        let hex = self.node.lock().await;
        let docs = hex.get_all_docs(tess);
        Ok(docs.len())
    }

    /// Method to retrieve a document from the specified tessellation.
    /// This method reads the document from the hex node's vertices and
    /// deserializes it into a Document object.
    /// If the document is not found, it returns None.
    pub async fn get_document(&self, tess: &str, id: &str) -> Result<Option<Document>> {
        let hex = self.node.lock().await;
        let raw = hex.read_document(tess, id);
        drop(hex);

        if let Some(data) = raw {
            Ok(Some(serde_json::from_slice(&data)?))
        } else {
            Ok(None)
        }
    }

    /// Method to delete a document from the specified tessellation.
    /// This method removes the document from the hex node's vertices
    /// and sends a delete operation to the Write-Ahead Log (WAL).
    /// If the document is not found, it returns an error.
    pub async fn delete_document(&self, tess: &str, id: &str) -> Result<()> {
        let mut hex = self.node.lock().await;
        hex.delete_document(tess, id);
        drop(hex);

        if let Err(e) = self
            .wal_tx
            .send(Wal::Delete {
                tessellation: tess.to_string(),
                id: id.to_string(),
            })
            .await
        {
            warn!("❗ Failed to send WAL delete: {}", e);
        }

        Ok(())
    }    

    /// Private method to insert a document into the hex.
    /// This method is used internally and is not exposed to the public API.
    async fn insert_document(&self, doc: Document) -> Result<()> {
        let mut hex = self.node.lock().await;

        // Check if the tessellation exists, if not, create it
        if !hex.tessellation_exists(&doc.tessellation) {
            hex.create_tessellation(&doc.tessellation, "user");
        }

        let raw = serde_json::to_vec(&doc)?;

        // Write to the hex node's vertices
        hex.create_document(&doc.tessellation, &doc.id.to_string(), &raw);

        drop(hex);

        // Track the document size in the SSTable
        self.sst.track_document_size(&doc);

        debug!(
            "📝 Inserted document: {} into tessellation {}.",
            doc.id, doc.tessellation
        );

        if let Err(e) = self.wal_tx.send(Wal::Insert(doc)).await {
            warn!("❗ Failed to send WAL Insert operation: {}", e);
        }

        Ok(())
    }
    
    
}
