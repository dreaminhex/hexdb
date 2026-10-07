// HexDB Core Engine Module
//
// The engine ties together the in-memory hex (memtable and cache), the
// write-ahead log, the SSTables on disk, and the tessellation catalog.
//
// Write path: under the state lock, a write gets the next sequence number, is
// queued to the WAL (so WAL order equals sequence order), and is applied to
// the hex. The lock is then released and the caller waits until the WAL
// record is durable before the write is acknowledged.
//
// Read path: the hex holds the newest version of every unflushed document and
// a cache of recently used ones. On a miss, the newest version across the
// SSTables is read from disk and cached. Expired documents and tombstones read
// as "not found".
//
// Flush: dirty hex entries are written to new SSTables, the WAL is rotated,
// and older WAL segments are deleted only after the SSTables are durable.

use crate::{
    catalog::{validate_tessellation_name, Catalog, TessellationInfo},
    document::{infer_fields_from_json, CompactFields, Document, FieldValue},
    hex::{DocKey, Hex, IntegrityReport, Lookup},
    network::discovery::PeerHex,
    sst::{parse_document, CompactionStats, DiskLookup, SstEntry, SstStore},
    wal::{self, WalOp, WalRecord, WalWriter},
    HexConfig,
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use rand::seq::IndexedRandom;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, warn};
use ulid::Ulid;

/// The identity of this hex within its lattice, decided before the engine is built.
#[derive(Debug, Clone)]
pub struct HexIdentity {
    pub id: Ulid,
    pub name: String,
    pub hex_type: String,
}

/// Errors callers may want to tell apart (e.g. to choose an HTTP status).
#[derive(Debug, Clone, PartialEq)]
pub enum EngineError {
    NotFound(String),
    Invalid(String),
    Conflict(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::NotFound(m) | EngineError::Invalid(m) | EngineError::Conflict(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for EngineError {}

/// Result of a flush.
#[derive(Debug, Default, Clone, Copy)]
pub struct FlushStats {
    pub entries: usize,
    pub tessellations: usize,
    pub wal_segments_deleted: usize,
}

/// Point-in-time engine statistics.
#[derive(Debug, Clone)]
pub struct EngineStats {
    pub ram_budget_bytes: usize,
    pub memory_bytes: usize,
    pub memory_entries: usize,
    pub memory_live_entries: usize,
    pub dirty_entries: usize,
    pub dirty_bytes: usize,
    pub disk_bytes: u64,
    pub sst_files: usize,
    pub next_seq: u64,
    pub wal_floor: u64,
    pub vertices: Vec<VertexStats>,
}

#[derive(Debug, Clone)]
pub struct VertexStats {
    pub id: usize,
    pub bytes: usize,
    pub shards: usize,
    pub corrupt_found: u64,
    pub repaired: u64,
}

/// Per-tessellation statistics.
#[derive(Debug, Clone, Default)]
pub struct TessellationStats {
    pub document_count: usize,
    pub documents_in_memory: usize,
    pub documents_on_disk_only: usize,
    /// Stored size of each live document: uncompressed in memory, compressed on disk.
    pub sizes: Vec<usize>,
}

struct EngineState {
    hex: Hex,
    next_seq: u64,
}

/// The newest version of a document across memory and disk.
enum Version {
    Memory { seq: u64, live: bool, expired: bool, len: usize },
    Disk { seq: u64, live: bool, expired: bool, len: usize },
}

impl Version {
    fn seq(&self) -> u64 {
        match self {
            Version::Memory { seq, .. } | Version::Disk { seq, .. } => *seq,
        }
    }
    fn visible(&self) -> bool {
        match self {
            Version::Memory { live, expired, .. } | Version::Disk { live, expired, .. } => *live && !*expired,
        }
    }
}

pub struct HexDBEngine {
    pub config: HexConfig,
    pub id: Ulid,
    pub name: String,
    pub hex_type: String,
    pub version: String,
    pub start_datetime: DateTime<Utc>,
    pub peers: Arc<Mutex<Vec<PeerHex>>>,

    storage_dir: PathBuf,
    wal_dir: PathBuf,
    state: Mutex<EngineState>,
    sst: SstStore,
    catalog: std::sync::Mutex<Catalog>,
    wal: WalWriter,
    flush_lock: Mutex<()>,
    flush_needed: Notify,
    /// Sequence numbers below this are no longer in the WAL.
    wal_floor: AtomicU64,
    ram_budget: usize,
    flush_threshold: usize,
}

const MAX_WRITE_RETRIES: usize = 16;

impl HexDBEngine {
    /// Open (or create) the storage directory, recover from SSTables and the
    /// WAL, and start the WAL writer. Anything recovered from the WAL is flushed
    /// to SSTables before this returns.
    pub async fn open(config: HexConfig, identity: HexIdentity, key: &[u8]) -> Result<Self> {
        let storage_dir = config.storage_dir();
        let wal_dir = storage_dir.join("wal");
        std::fs::create_dir_all(&storage_dir)
            .with_context(|| format!("Failed to create {}", storage_dir.display()))?;

        let legacy = wal::legacy_wal_files(&storage_dir);
        if !legacy.is_empty() {
            bail!(
                "Found WAL files from an older HexDB build in {} ({}). This build can't read them; move them out of the data directory to start.",
                storage_dir.display(),
                legacy.iter().filter_map(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).collect::<Vec<_>>().join(", ")
            );
        }

        let level = config.compression.compression_level;
        let mut catalog = Catalog::load(&storage_dir)?.unwrap_or_default();
        let sst = SstStore::open(&storage_dir, level)?;

        // Clean up tables left behind by a drop that was interrupted.
        for (tess, &dropped_seq) in &catalog.dropped {
            sst.purge_dropped(tess, dropped_seq).await?;
        }
        // Register tessellations found on disk but missing from the catalog.
        let mut catalog_changed = false;
        for tess in sst.tessellations().await {
            if !catalog.tessellations.contains_key(&tess) {
                catalog.tessellations.insert(
                    tess.clone(),
                    TessellationInfo { kind: Catalog::default_kind(&tess).into(), created: Utc::now().timestamp_millis() },
                );
                catalog_changed = true;
            }
        }

        // Replay the WAL.
        let mut records = Vec::new();
        let replay = wal::replay(&wal_dir, key, |r| records.push(r))?;
        if replay.corrupt_records > 0 {
            bail!(
                "{} WAL record(s) in {} could not be decrypted or decoded. This usually means storage.encryption_key has changed. HexDB will not start, so the records aren't discarded.",
                replay.corrupt_records,
                wal_dir.display()
            );
        }

        let mut hex = Hex::new();
        let mut applied = 0usize;
        for record in records {
            let (tess, id) = match &record.op {
                WalOp::Put(doc) => (doc.tessellation.clone(), doc.id),
                WalOp::Delete { tessellation, id } => (tessellation.clone(), *id),
            };
            if catalog.dropped.get(&tess).is_some_and(|&d| record.seq <= d) {
                continue;
            }
            if sst.seq_of(&tess, &id).await.is_some_and(|s| s >= record.seq) {
                continue;
            }
            let key = DocKey::new(&tess, id);
            if hex.meta(&key).is_some_and(|m| m.seq >= record.seq) {
                continue;
            }
            match record.op {
                WalOp::Put(doc) => hex.put(&key, record.seq, doc.ttl, &serde_json::to_vec(&doc)?, true),
                WalOp::Delete { .. } => hex.put_tombstone(&key, record.seq, true),
            }
            if !catalog.tessellations.contains_key(&tess) {
                catalog.tessellations.insert(
                    tess.clone(),
                    TessellationInfo { kind: Catalog::default_kind(&tess).into(), created: Utc::now().timestamp_millis() },
                );
                catalog_changed = true;
            }
            applied += 1;
        }
        if catalog_changed {
            catalog.save(&storage_dir)?;
        }

        let next_seq = sst
            .max_seq()
            .await
            .max(replay.max_seq)
            .max(catalog.max_dropped_seq())
            + 1;
        let wal_floor = wal::list_segments(&wal_dir)?
            .first()
            .map(|(seq, _)| *seq)
            .unwrap_or(next_seq);

        info!(
            "🔁 Recovered {} of {} WAL record(s) from {} segment(s){}. Next sequence number: {}.",
            applied,
            replay.records,
            replay.segments,
            if replay.torn_tails > 0 { format!(", ignoring {} incomplete record(s)", replay.torn_tails) } else { String::new() },
            next_seq
        );

        let wal = WalWriter::start(&wal_dir, key, level, config.storage.wal_sync, next_seq)?;
        let ram_budget = (config.memory.ram_mb as usize).saturating_mul(1024 * 1024).max(1024 * 1024);

        let engine = HexDBEngine {
            id: identity.id,
            name: identity.name,
            hex_type: identity.hex_type,
            version: env!("CARGO_PKG_VERSION").to_string(),
            start_datetime: Utc::now(),
            peers: Arc::new(Mutex::new(Vec::new())),
            config,
            storage_dir,
            wal_dir,
            state: Mutex::new(EngineState { hex, next_seq }),
            sst,
            catalog: std::sync::Mutex::new(catalog),
            wal,
            flush_lock: Mutex::new(()),
            flush_needed: Notify::new(),
            wal_floor: AtomicU64::new(wal_floor),
            ram_budget,
            flush_threshold: ram_budget / 4,
        };

        // Move recovered writes into SSTables and retire the old WAL segments.
        if replay.segments > 0 {
            engine.flush().await.context("Failed to flush recovered WAL records")?;
        }

        Ok(engine)
    }

    /// Picks a random name for the Hex from the provided list of names.
    pub fn pick_random_name(names: &[String]) -> Option<String> {
        let mut rng = rand::rng();
        names.choose(&mut rng).cloned()
    }

    /// Signalled when enough unflushed data has accumulated to warrant a flush.
    pub fn flush_needed(&self) -> &Notify {
        &self.flush_needed
    }

    // -----------------------------------------------------------------------
    // Tessellations
    // -----------------------------------------------------------------------

    /// All tessellations as (name, kind).
    pub fn tessellations(&self) -> Vec<(String, String)> {
        self.catalog
            .lock()
            .unwrap()
            .tessellations
            .iter()
            .map(|(name, info)| (name.clone(), info.kind.clone()))
            .collect()
    }

    pub fn tessellation_exists(&self, name: &str) -> bool {
        self.catalog.lock().unwrap().tessellations.contains_key(name)
    }

    /// Create a tessellation. Returns false if it already exists.
    pub fn create_tessellation(&self, name: &str, kind: &str) -> Result<bool> {
        validate_tessellation_name(name).map_err(|e| EngineError::Invalid(e.to_string()))?;
        let mut catalog = self.catalog.lock().unwrap();
        if let Some(existing) = catalog.find_case_insensitive(name) {
            if existing == name {
                return Ok(false);
            }
            return Err(EngineError::Conflict(format!(
                "Tessellation '{}' conflicts with existing tessellation '{}' (names are case-insensitive).",
                name, existing
            ))
            .into());
        }
        catalog.tessellations.insert(
            name.to_string(),
            TessellationInfo { kind: kind.to_string(), created: Utc::now().timestamp_millis() },
        );
        catalog.save(&self.storage_dir)?;
        info!("🧩 Created tessellation '{}' ({}).", name, kind);
        Ok(true)
    }

    fn ensure_tessellation(&self, name: &str) -> Result<()> {
        if !self.tessellation_exists(name) {
            self.create_tessellation(name, Catalog::default_kind(name))?;
        }
        Ok(())
    }

    /// Delete a tessellation and all of its documents. Returns false if it doesn't exist.
    pub async fn delete_tessellation(&self, name: &str) -> Result<bool> {
        if !self.tessellation_exists(name) {
            return Ok(false);
        }
        // No flush may write this tessellation's SSTables while it is dropped.
        let _flush = self.flush_lock.lock().await;
        {
            let mut state = self.state.lock().await;
            let drop_seq = state.next_seq;
            state.next_seq += 1;
            state.hex.remove_tessellation(name);

            let mut catalog = self.catalog.lock().unwrap();
            catalog.tessellations.remove(name);
            catalog.dropped.insert(name.to_string(), drop_seq);
            catalog.save(&self.storage_dir)?;
        }
        self.sst.drop_tessellation(name).await?;
        info!("🗑️ Deleted tessellation '{}'.", name);
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // Documents
    // -----------------------------------------------------------------------

    /// Insert a JSON object as a new document. `ttl` is an expiry time in epoch milliseconds.
    /// An `id` field in the input is ignored; IDs are assigned by the server.
    pub async fn insert_json(&self, tess: &str, json: Value, ttl: Option<i64>) -> Result<Document> {
        validate_tessellation_name(tess).map_err(|e| EngineError::Invalid(e.to_string()))?;
        let mut data = object_fields(&json)?;
        data.remove("id");
        self.ensure_tessellation(tess)?;

        let doc = Document { id: Ulid::new(), tessellation: tess.to_string(), data, ttl };
        let key = DocKey::new(tess, doc.id);
        self.write(&key, WalOp::Put(doc.clone()), None).await?;
        debug!("✅ Document {} inserted into tessellation {}.", doc.id, tess);
        Ok(doc)
    }

    /// Replace a document's fields. `json` must contain the document's `id`.
    /// The TTL is kept unless a new one is given.
    pub async fn update_json(&self, tess: &str, json: Value, ttl: Option<i64>) -> Result<Document> {
        let mut fields = object_fields(&json)?;
        fields.remove("id");
        self.modify(tess, &json, ttl, |doc| doc.data = fields.clone()).await
    }

    /// Merge fields into a document. `json` must contain the document's `id`.
    /// A field set to `null` is removed (JSON Merge Patch semantics, top level only).
    pub async fn patch_json(&self, tess: &str, json: Value, ttl: Option<i64>) -> Result<Document> {
        let Value::Object(map) = &json else {
            return Err(EngineError::Invalid("Expected a JSON object.".into()).into());
        };
        let changes: Vec<(String, Option<FieldValue>)> = map
            .iter()
            .filter(|(k, _)| k.as_str() != "id")
            .map(|(k, v)| (k.clone(), if v.is_null() { None } else { Some(FieldValue::from_json(v)) }))
            .collect();

        self.modify(tess, &json, ttl, |doc| {
            for (k, v) in &changes {
                match v {
                    Some(v) => {
                        doc.data.insert(k.clone(), v.clone());
                    }
                    None => {
                        doc.data.remove(k);
                    }
                }
            }
        })
        .await
    }

    /// Read-modify-write with optimistic concurrency: retried if the document
    /// changes between the read and the write.
    async fn modify(&self, tess: &str, json: &Value, ttl: Option<i64>, change: impl Fn(&mut Document)) -> Result<Document> {
        let id_str = json
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| EngineError::Invalid("Missing 'id' field.".into()))?;
        let id = Ulid::from_string(id_str).map_err(|_| EngineError::NotFound(format!("Document {} not found.", id_str)))?;
        let key = DocKey::new(tess, id);

        for _ in 0..MAX_WRITE_RETRIES {
            let (current, seq) = self.read_latest(&key).await?;
            let Some(mut doc) = current else {
                return Err(EngineError::NotFound(format!("Document {} not found in '{}'.", id, tess)).into());
            };
            change(&mut doc);
            if ttl.is_some() {
                doc.ttl = ttl;
            }
            if self.write(&key, WalOp::Put(doc.clone()), Some(seq)).await? {
                debug!("📝 Document {} updated in tessellation {}.", id, tess);
                return Ok(doc);
            }
        }
        Err(EngineError::Conflict(format!("Document {} is being modified concurrently; try again.", id)).into())
    }

    /// Fetch a document. Returns `None` for unknown, deleted or expired documents.
    pub async fn get_document(&self, tess: &str, id: &str) -> Result<Option<Document>> {
        let Ok(id) = Ulid::from_string(id) else { return Ok(None) };
        if validate_tessellation_name(tess).is_err() {
            return Ok(None);
        }
        Ok(self.read_latest(&DocKey::new(tess, id)).await?.0)
    }

    /// Delete a document. Returns false if it didn't exist.
    pub async fn delete_document(&self, tess: &str, id: &str) -> Result<bool> {
        let Ok(id) = Ulid::from_string(id) else { return Ok(false) };
        if validate_tessellation_name(tess).is_err() {
            return Ok(false);
        }
        let key = DocKey::new(tess, id);
        for _ in 0..MAX_WRITE_RETRIES {
            let (current, seq) = self.read_latest(&key).await?;
            if current.is_none() {
                return Ok(false);
            }
            let op = WalOp::Delete { tessellation: tess.to_string(), id };
            if self.write(&key, op, Some(seq)).await? {
                debug!("🗑️ Document {} deleted from tessellation {}.", id, tess);
                return Ok(true);
            }
        }
        Err(EngineError::Conflict(format!("Document {} is being modified concurrently; try again.", id)).into())
    }

    /// Number of visible documents in a tessellation.
    pub async fn count_documents(&self, tess: &str) -> Result<usize> {
        Ok(self.versions(tess).await.values().filter(|v| v.visible()).count())
    }

    /// Visible documents in a tessellation, ordered by ID.
    pub async fn list_documents(&self, tess: &str, offset: usize, limit: usize) -> Result<Vec<Document>> {
        let mut ids: Vec<Ulid> = self
            .versions(tess)
            .await
            .into_iter()
            .filter(|(_, v)| v.visible())
            .map(|(id, _)| id)
            .collect();
        ids.sort();

        let mut docs = Vec::new();
        for id in ids.into_iter().skip(offset).take(limit) {
            if let Some(doc) = self.read_latest(&DocKey::new(tess, id)).await?.0 {
                docs.push(doc);
            }
        }
        Ok(docs)
    }

    /// Per-tessellation statistics.
    pub async fn tessellation_stats(&self, tess: &str) -> TessellationStats {
        let mut stats = TessellationStats::default();
        for version in self.versions(tess).await.values() {
            if !version.visible() {
                continue;
            }
            stats.document_count += 1;
            match version {
                Version::Memory { len, .. } => {
                    stats.documents_in_memory += 1;
                    stats.sizes.push(*len);
                }
                Version::Disk { len, .. } => {
                    stats.documents_on_disk_only += 1;
                    stats.sizes.push(*len);
                }
            }
        }
        stats
    }

    /// The newest version of every document in a tessellation.
    async fn versions(&self, tess: &str) -> HashMap<Ulid, Version> {
        let now = Utc::now().timestamp_millis();
        let state = self.state.lock().await;
        let disk = self.sst.latest_entries(tess).await;
        let memory = state.hex.entries_in(tess);
        drop(state);

        let mut versions: HashMap<Ulid, Version> = disk
            .into_iter()
            .map(|(id, (_file, entry))| {
                (
                    id,
                    Version::Disk {
                        seq: entry.seq,
                        live: !entry.tombstone,
                        expired: entry.is_expired(now),
                        len: entry.len as usize,
                    },
                )
            })
            .collect();
        for (key, meta) in memory {
            if versions.get(&key.id).is_none_or(|v| v.seq() <= meta.seq) {
                versions.insert(
                    key.id,
                    Version::Memory { seq: meta.seq, live: !meta.tombstone, expired: meta.is_expired(now), len: meta.len },
                );
            }
        }
        versions
    }

    /// Read the newest version of a document. Returns the document (if visible)
    /// and the sequence number of the version seen (0 if none).
    async fn read_latest(&self, key: &DocKey) -> Result<(Option<Document>, u64)> {
        let now = Utc::now().timestamp_millis();
        {
            let mut state = self.state.lock().await;
            match state.hex.read(key) {
                Some(Lookup::Live { bytes, seq, ttl }) => {
                    if ttl.is_some_and(|t| t <= now) {
                        return Ok((None, seq));
                    }
                    return Ok((Some(parse_document(&bytes)?), seq));
                }
                Some(Lookup::Tombstone { seq }) => return Ok((None, seq)),
                Some(Lookup::Unrecoverable { seq }) => {
                    let dirty = state.hex.meta(key).is_some_and(|m| m.dirty);
                    if dirty {
                        bail!(
                            "Document {} in '{}' is corrupt in memory on too many vertices to rebuild. It will be restored from the WAL on restart.",
                            key.id,
                            key.tessellation
                        );
                    }
                    error!("❌ Document {} in '{}' is corrupt in memory; reloading it from disk.", key.id, key.tessellation);
                    let _ = seq;
                    state.hex.remove(key);
                }
                None => {}
            }
        }

        for attempt in 0..3 {
            match self.sst.lookup(&key.tessellation, &key.id).await {
                None => return Ok((None, 0)),
                Some(DiskLookup::Tombstone { seq }) => return Ok((None, seq)),
                Some(DiskLookup::Live { file, entry }) => {
                    if entry.is_expired(now) {
                        return Ok((None, entry.seq));
                    }
                    let read = {
                        let file = file.clone();
                        tokio::task::spawn_blocking(move || file.read_entry(&entry)).await?
                    };
                    match read {
                        Ok(bytes) => {
                            let doc = parse_document(&bytes)?;
                            self.cache(key, entry.seq, entry.ttl, &bytes).await;
                            return Ok((Some(doc), entry.seq));
                        }
                        // The file may have been replaced by compaction; look again.
                        Err(e) if attempt < 2 && !file.path.exists() => {
                            debug!("Retrying read after compaction: {}", e);
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        bail!("Document {} in '{}' could not be read.", key.id, key.tessellation)
    }

    /// Cache a document read from disk, unless a newer version appeared meanwhile.
    async fn cache(&self, key: &DocKey, seq: u64, ttl: Option<i64>, bytes: &[u8]) {
        let mut state = self.state.lock().await;
        if state.hex.meta(key).is_some() {
            return;
        }
        if self.sst.seq_of(&key.tessellation, &key.id).await != Some(seq) {
            return;
        }
        state.hex.put(key, seq, ttl, bytes, false);
        if state.hex.memory_bytes() > self.ram_budget {
            state.hex.evict_clean(self.ram_budget * 3 / 4);
        }
    }

    /// Apply a write. With `expected_seq`, the write only happens if the
    /// document's newest version still has that sequence number; returns
    /// false if it changed. Returns once the write is durable in the WAL.
    async fn write(&self, key: &DocKey, op: WalOp, expected_seq: Option<u64>) -> Result<bool> {
        if self.wal.has_failed() {
            bail!("Writes are disabled after a WAL failure. Check the disk and restart HexDB.");
        }
        let bytes = match &op {
            WalOp::Put(doc) => Some(serde_json::to_vec(doc)?),
            WalOp::Delete { .. } => None,
        };
        let ttl = match &op {
            WalOp::Put(doc) => doc.ttl,
            WalOp::Delete { .. } => None,
        };

        let ack = {
            let mut state = self.state.lock().await;
            if let Some(expected) = expected_seq {
                let current = match state.hex.meta(key) {
                    Some(meta) => meta.seq,
                    None => self.sst.seq_of(&key.tessellation, &key.id).await.unwrap_or(0),
                };
                if current != expected {
                    return Ok(false);
                }
            }

            let seq = state.next_seq;
            // Queue to the WAL first so nothing is applied if the WAL is unavailable.
            let ack = self.wal.append(WalRecord { seq, op }).await?;
            state.next_seq += 1;
            match &bytes {
                Some(bytes) => state.hex.put(key, seq, ttl, bytes, true),
                None => state.hex.put_tombstone(key, seq, true),
            }
            if state.hex.dirty_bytes() >= self.flush_threshold {
                self.flush_needed.notify_one();
            }
            ack
        };

        ack.wait().await?;
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // Maintenance
    // -----------------------------------------------------------------------

    /// Write all unflushed documents to SSTables, then delete WAL segments
    /// that are no longer needed.
    pub async fn flush(&self) -> Result<FlushStats> {
        let _guard = self.flush_lock.lock().await;

        let (snapshot, rotation) = {
            let mut state = self.state.lock().await;
            if state.hex.dirty_count() == 0 {
                return Ok(FlushStats::default());
            }
            let snapshot = state.hex.dirty_snapshot().map_err(|key| {
                anyhow!(
                    "Flush aborted: unflushed document {} in '{}' is corrupt on too many vertices. It will be restored from the WAL on restart.",
                    key.id,
                    key.tessellation
                )
            })?;
            // Everything queued before this point lands in the old segment.
            let rotation = self.wal.rotate(state.next_seq).await?;
            (snapshot, rotation)
        };
        let new_segment = rotation
            .await
            .map_err(|_| anyhow!("WAL writer stopped during rotation"))?
            .map_err(|e| anyhow!("WAL rotation failed: {}", e))?;

        let mut by_tess: BTreeMap<String, Vec<SstEntry>> = BTreeMap::new();
        let mut flushed = Vec::with_capacity(snapshot.len());
        for entry in snapshot {
            flushed.push((entry.key.clone(), entry.seq));
            by_tess.entry(entry.key.tessellation.clone()).or_default().push(SstEntry {
                id: entry.key.id,
                seq: entry.seq,
                ttl: entry.ttl,
                data: entry.bytes,
            });
        }

        let mut stats = FlushStats { entries: flushed.len(), tessellations: by_tess.len(), ..Default::default() };
        for (tess, entries) in by_tess {
            self.sst
                .add_table(&tess, entries)
                .await
                .with_context(|| format!("Failed to write SSTable for '{}'", tess))?;
        }

        {
            let mut state = self.state.lock().await;
            state.hex.mark_clean(&flushed);
            if state.hex.memory_bytes() > self.ram_budget {
                let evicted = state.hex.evict_clean(self.ram_budget * 3 / 4);
                debug!("🧠 Evicted {} cached documents after flush.", evicted);
            }
        }

        stats.wal_segments_deleted = wal::delete_segments_before(&self.wal_dir, new_segment)?;
        self.wal_floor.store(new_segment, Ordering::SeqCst);
        info!("💾 Flushed {} entries across {} tessellation(s) to SSTables.", stats.entries, stats.tessellations);
        Ok(stats)
    }

    /// Merge SSTables and drop tombstones and expired documents that are safe to drop.
    pub async fn compact(&self) -> Result<CompactionStats> {
        let stats = self
            .sst
            .compact(self.wal_floor.load(Ordering::SeqCst), Utc::now().timestamp_millis())
            .await?;
        if stats.files_merged > 0 {
            info!(
                "🗜️  Compacted {} SSTables in {} tessellation(s): kept {}, dropped {}.",
                stats.files_merged, stats.tessellations, stats.entries_kept, stats.entries_dropped
            );
        }
        Ok(stats)
    }

    /// Evict expired documents from memory. Returns how many were evicted.
    pub async fn sweep_expired(&self) -> usize {
        let removed = self.state.lock().await.hex.evict_expired_clean(Utc::now().timestamp_millis());
        if removed > 0 {
            info!("🧹 TTL sweep evicted {} expired documents from memory.", removed);
        }
        removed
    }

    /// Evict cached documents if memory use is over budget.
    pub async fn enforce_memory_budget(&self) -> usize {
        let mut state = self.state.lock().await;
        if state.hex.memory_bytes() > self.ram_budget {
            state.hex.evict_clean(self.ram_budget * 3 / 4)
        } else {
            0
        }
    }

    /// Verify every document's shards across the vertices and repair corrupt ones.
    pub async fn check_vertices(&self) -> IntegrityReport {
        let keys = self.state.lock().await.hex.keys();
        let mut total = IntegrityReport::default();
        for chunk in keys.chunks(512) {
            let report = self.state.lock().await.hex.check_integrity(chunk);
            total.checked += report.checked;
            total.corrupt_shards += report.corrupt_shards;
            total.repaired_shards += report.repaired_shards;
            total.unrecoverable += report.unrecoverable;
        }
        if total.corrupt_shards > 0 {
            warn!(
                "🩹 Vertex check: {} corrupt shard(s), {} repaired, {} document(s) unrecoverable in memory.",
                total.corrupt_shards, total.repaired_shards, total.unrecoverable
            );
        }
        total
    }

    /// Flush everything and stop the WAL writer. Call once, after the server stops accepting requests.
    pub async fn shutdown(&self) -> Result<()> {
        let flushed = self.flush().await;
        if let Err(e) = &flushed {
            error!("❌ Final flush failed; unflushed writes remain in the WAL: {:#}", e);
        }
        self.wal.shutdown().await?;
        flushed.map(|_| ())
    }

    /// Point-in-time statistics.
    pub async fn stats(&self) -> EngineStats {
        let (disk_bytes, sst_files) = self.sst.disk_usage().await;
        let state = self.state.lock().await;
        EngineStats {
            ram_budget_bytes: self.ram_budget,
            memory_bytes: state.hex.memory_bytes(),
            memory_entries: state.hex.entry_count(),
            memory_live_entries: state.hex.live_count(),
            dirty_entries: state.hex.dirty_count(),
            dirty_bytes: state.hex.dirty_bytes(),
            disk_bytes,
            sst_files,
            next_seq: state.next_seq,
            wal_floor: self.wal_floor.load(Ordering::SeqCst),
            vertices: state
                .hex
                .vertices
                .iter()
                .map(|v| VertexStats {
                    id: v.id,
                    bytes: v.bytes(),
                    shards: v.shard_count(),
                    corrupt_found: v.corrupt_found,
                    repaired: v.repaired,
                })
                .collect(),
        }
    }

    /// Corrupt one vertex's shard of a document held in memory. Testing aid only.
    #[doc(hidden)]
    pub async fn corrupt_shard_for_testing(&self, tess: &str, id: Ulid, vertex: usize) -> bool {
        self.state.lock().await.hex.corrupt_for_testing(&DocKey::new(tess, id), vertex)
    }
}

fn object_fields(json: &Value) -> Result<CompactFields> {
    if !json.is_object() {
        return Err(EngineError::Invalid("Expected a JSON object.".into()).into());
    }
    Ok(infer_fields_from_json(json))
}
