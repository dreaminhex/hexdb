// HexDB Core Memory Engine Module
// This module implements a memory-based engine for HexDB, allowing for
// in-memory storage and retrieval of documents. The memory engine is designed
// to be fast and efficient. It uses a hash map to store documents, and provides
// methods for inserting, updating, deleting, and retrieving documents.

use crate::{
    document::{infer_fields_from_json, Document}, engine::Engine, hex::HexNode, sst::{SstReader, SstWriter}, wal::Wal, HexConfig
};
use anyhow::{Result, bail, Context};
use chrono::{DateTime, Utc};
use rand::seq::IndexedRandom;
use serde_json::Value;
use tracing::{debug, info, warn};
use std::{collections::HashMap, path::{Path, PathBuf}, sync::Arc};
use dashmap::DashMap;
use tokio::{fs::{self, File}, io::{self, AsyncBufReadExt, BufReader}, sync::{mpsc::Sender, Mutex}};
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
    pub config: HexConfig,
    pub wal_tx: Sender<Wal>,
    pub id: Ulid,
    pub name: String,
    pub hex_type: String,
    pub version: String,
    pub start_datetime: DateTime<Utc>
}

impl MemoryEngine {
    pub async fn new(wal_tx: Sender<Wal>, config: HexConfig) -> Self {

        let names = Self::load_names("./data/names.txt").await.unwrap_or_default();
        let name = Self::pick_random_name(&names).unwrap_or_else(|| "Unnamed Hex".to_string());

        Self {
            store: Arc::new(DashMap::new()),
            node: Arc::new(Mutex::new(HexNode::new())),
            tess_map: Arc::new(DashMap::new()),
            total_doc_bytes: Arc::new(AtomicUsize::new(0)),
            doc_count: Arc::new(AtomicUsize::new(0)),
            config,
            wal_tx,
            id: Ulid::new(),
            name,
            hex_type: "manager".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            start_datetime: Utc::now(),
        }
    }

    pub async fn compact_all(&self) -> Result<()> {
        let base = PathBuf::from("./.hexdb");
        if !base.exists() {
            return Ok(());
        }

        let mut dirs = fs::read_dir(&base).await?;
        while let Some(entry) = dirs.next_entry().await? {
            let path = entry.path();
            if path.is_dir() {
                if let Some(tess) = path.file_name().and_then(|n| n.to_str()) {
                    self.compact_sstables(tess).await?;
                }
            }
        }

        Ok(())
    }

    pub async fn compact_sstables(&self, tess: &str) -> Result<()> {

        let dir = PathBuf::from(format!("./.hexdb/{}", tess));
        if !dir.exists() {
            return Ok(());
        }

        let now = Utc::now().timestamp_millis();
        let mut all_docs: HashMap<Ulid, Document> = HashMap::new();
        let mut to_delete = Vec::new();

        let mut files = fs::read_dir(&dir).await?;
        while let Some(entry) = files.next_entry().await? {
            let path = entry.path();
            if path.extension().map_or(false, |ext| ext == "hxs") {
                let map = SstReader::load_all(&path)?;
                let mut retained = 0;
                for (id, doc) in map {
                    if doc.ttl.map_or(false, |ttl| ttl < now) {
                        continue;
                    }
                    all_docs.insert(id, doc);
                    retained += 1;
                }

                // Only delete if docs were valid and contributed
                if retained > 0 {
                    to_delete.push(path);
                }
            }
        }

        if all_docs.is_empty() {
            debug!("🧹 No active documents found in SSTables for '{}'.", tess);
            return Ok(());
        }

        let compact_path = dir.join(format!("{}.hxs", Ulid::new()));
        SstWriter::write(self.config.clone(), &compact_path, all_docs.clone().into_iter().collect())?;

        // Remove old files
        for path in &to_delete {
            debug!("🗑️  Deleted obsolete SSTable file after compaction: {:?}", path);
            let _ = fs::remove_file(path).await;
        }

        info!("🔧 Compaction check - Found {} SSTables for '{}'. Compacted, retained {} docs.", to_delete.len(), tess, all_docs.len());

        Ok(())
    }

    pub async fn load_sstables(&self) -> Result<()> {
        let base = PathBuf::from("./.hexdb");
        if !base.exists() {
            return Ok(());
        }

        let mut tess_dirs = fs::read_dir(&base).await?;

        while let Some(tess_entry) = tess_dirs.next_entry().await? {
            if !tess_entry.file_type().await?.is_dir() {
                continue;
            }

            let tess_name = tess_entry.file_name().to_string_lossy().to_string();
            let dir_path = tess_entry.path();

            info!("📂 Loading SSTables for tessellation '{}'", tess_name);

            let mut files = fs::read_dir(&dir_path).await?;
            while let Some(entry) = files.next_entry().await? {
                let path = entry.path();
                if path.extension().map_or(false, |ext| ext == "hxs") {
                    let map = SstReader::load_all(&path)?;
                    for (id, doc) in map {
                        if let Some(ttl) = doc.ttl {
                            if ttl < Utc::now().timestamp_millis() {
                                continue;
                            }
                        }
                        self.track_doc_size(&doc);
                        self.tess_map.insert(id.to_string(), doc.tessellation.clone());
                        self.store.insert(id.to_string(), doc);
                    }
                }
            }
        }

        info!("✅ SSTables loaded into memory.");
        Ok(())
    }
   
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

    pub fn pick_random_name(names: &[String]) -> Option<String> {
        let mut rng = rand::rng();
        names.choose(&mut rng).cloned()
    }

    pub fn adaptive_max_docs(&self) -> usize {
        let ram_docs = self.doc_count.load(Ordering::Relaxed);
        let used_ram = self.total_doc_bytes.load(Ordering::Relaxed);
        let disk_docs = self.enumerate_disk_documents().unwrap_or(0);
        let ram_mb = self.config.memory.ram_mb;
        let ram_bytes = ram_mb as usize * 1024 * 1024;

        let avg_size = if ram_docs > 0 {
            used_ram / ram_docs
        } else {
            0
        };

        let max_docs = if avg_size > 0 {
            ram_bytes / avg_size
        } else {
            usize::MAX // can't calculate if avg_size is 0
        };

        let ram_pct = if ram_bytes > 0 {
            (used_ram as f64 / ram_bytes as f64) * 100.0
        } else {
            0.0
        };

        debug!(
            ram_mb,
            ram_bytes,
            used_ram,
            ram_pct,
            avg_size,
            ram_docs,
            disk_docs,
            max_docs,
            "🧠 Memory status: {:.2} KB used / {} MB total ({:.2}%). Documents: In RAM: {}. On Disk: {}. Average doc size is {} bytes. Flush triggers at ~{} docs (~{} MB).",
            used_ram as f64 / 1024.0,
            ram_mb,
            ram_pct,
            ram_docs,
            disk_docs,
            avg_size,
            max_docs,
            max_docs * avg_size / 1024 / 1024
        );

        max_docs
    }

    pub fn enumerate_disk_documents(&self) -> Result<usize> {
        let base = Path::new("./.hexdb");
        if !base.exists() {
            return Ok(0);
        }

        let mut total = 0;
        for tess_entry in std::fs::read_dir(base)? {
            let tess_path = tess_entry?.path();
            if !tess_path.is_dir() {
                continue;
            }

            for file in std::fs::read_dir(tess_path)? {
                let path = file?.path();
                if path.extension().map_or(false, |ext| ext == "hxs") {
                    let map = SstReader::load_all(&path)?;
                    total += map.len();
                }
            }
        }

        Ok(total)
    }

    pub fn used_ram_bytes(&self) -> usize {
        self.total_doc_bytes.load(Ordering::Relaxed)
    }

    pub async fn sweep_expired_documents(&self) -> Result<usize> {
        
        info!("🧹 Starting TTL sweep...");

        let now = Utc::now().timestamp_millis();
        let mut removed = 0;

        // Memory sweep
        for entry in self.store.iter() {
            if let Some(ttl) = entry.value().ttl {
                if ttl < now {
                    self.store.remove(entry.key());
                    self.tess_map.remove(entry.key());
                    removed += 1;
                }
            }
        }

        // Disk sweep by compacting each tessellation
        let base = PathBuf::from("./.hexdb");
        if base.exists() {
            let mut dirs = fs::read_dir(&base).await?;
            while let Some(entry) = dirs.next_entry().await? {
                let path = entry.path();
                if path.is_dir() {
                    if let Some(tess) = path.file_name().and_then(|n| n.to_str()) {
                        self.compact_sstables(tess).await?;
                    }
                }
            }
        }

        if removed > 0 {
            info!("🧹 TTL sweep removed {} expired in-memory docs.", removed);
        }

        Ok(removed)
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

    pub async fn flush_to_sstable(&self, wal_tx: &Sender<Wal>) -> Result<()> {
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

            let dir = PathBuf::from(format!("./.hexdb/{}", tess));
            fs::create_dir_all(&dir).await?;

            let filename = format!("{}.hxs", Ulid::new());
            let path = dir.join(filename);

            SstWriter::write(self.config.clone(), &path, docs.clone())
                .with_context(|| format!("❌ Failed to write SSTable for {}.", tess))?;

            for (id, _) in docs {
                self.store.remove(&id.to_string());
                self.tess_map.remove(&id.to_string());
            }

            // Compact the SSTable            
            self.compact_sstables(&tess).await?;
        }

        self.total_doc_bytes.store(0, Ordering::Relaxed);
        self.doc_count.store(0, Ordering::Relaxed);

        // Rotate WAL file
        wal_tx.send(Wal::Rotate).await.ok();

        // Reload the hot cache.
        self.reload_cache()?;

        info!("✅ Flushed documents to SSTable successfully.");
        Ok(())
    }
   
    pub fn reload_cache(&self) -> Result<()> {
        let base = Path::new("./.hexdb");
        if !base.exists() {
            return Ok(());
        }

        for tess_entry in std::fs::read_dir(base)? {
            let tess_path = tess_entry?.path();
            if !tess_path.is_dir() {
                continue;
            }

            let mut entries: Vec<_> = std::fs::read_dir(&tess_path)?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map_or(false, |ext| ext == "hxs"))
                .collect();

            // Sort by most recent modified time
            entries.sort_by_key(|e| std::fs::metadata(e.path()).and_then(|m| m.modified()).ok());
            entries.reverse();

            for entry in entries.iter().take(1) {
                let path = entry.path();
                if let Ok(map) = SstReader::load_all(&path) {
                    for (_, doc) in map {
                        if let Some(ttl) = doc.ttl {
                            if ttl < Utc::now().timestamp_millis() {
                                continue;
                            }
                        }
                        self.track_doc_size(&doc);
                        self.tess_map.insert(doc.id.to_string(), doc.tessellation.clone());
                        self.store.insert(doc.id.to_string(), doc);
                    }
                }
            }
        }

        info!("🔥 Hot cache reload complete.");
        Ok(())
    }

    pub fn track_doc_size(&self, doc: &Document) {
        if let Ok(json) = serde_json::to_vec(doc) {
            let len = json.len();
            self.total_doc_bytes.fetch_add(len, Ordering::Relaxed);
            self.doc_count.fetch_add(1, Ordering::Relaxed);
        }
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

        let dir = PathBuf::from(format!("./.hexdb/{}", tess));
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
