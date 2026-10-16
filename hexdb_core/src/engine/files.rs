// HexDB Core Engine: binary files
//
// Uploads, downloads, the trash, versions, and garbage collection for files
// tessellations (see `crate::files` for the document shape and
// `crate::blobs` for the chunk store).
//
// An upload streams the body through the chunker into the chunk store, writes
// a manifest chunk, makes the chunks durable, and only then commits the file's
// document. A crash before the commit leaves unreferenced chunks, which the
// next garbage collection removes. Uploading to a path that already has a file
// makes a new version of it; the previous one is kept in `versions`.
//
// Document writes through the generic routes go through `guard_file_writes`:
// system fields keep their stored values, new documents can't be created
// (files are uploaded), and a delete moves the document to the trash.

use super::{writes::BatchItem, EngineError, HexDBEngine, MAX_WRITE_RETRIES};
use crate::{
    auth::Action,
    blobs::{self, ChunkId, Pins, ScrubReport, SweepReport},
    catalog::TessellationInfo,
    document::{infer_fields_from_json, Document},
    files::{self, Chunker, Manifest, FILES_KIND, PATH_INDEX, SYSTEM_FIELDS, TRASH_FIELDS, TRASH_TESSELLATION, UPLOADS_TESSELLATION},
    filter::Filter,
    hex::DocKey,
    index::{IndexDef, IndexKind},
    wal::WalOp,
};
use anyhow::{anyhow, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use futures::{Stream, StreamExt};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, sync::Arc, sync::atomic::Ordering, time::Duration};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};
use ulid::Ulid;

pub(crate) mod ai_files;
pub(crate) mod quotas;

/// What the client says about an upload.
#[derive(Debug, Clone, Default)]
pub struct UploadOptions {
    /// The declared content type (used when sniffing finds nothing better).
    pub content_type: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub modified_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct UploadResult {
    pub document: Document,
    /// True for a new file, false for a new version of an existing one.
    pub created: bool,
    /// True if the content was identical to the current version (nothing stored).
    pub unchanged: bool,
}

/// A file's content, stored and ready to commit.
struct Staged {
    manifest: Manifest,
    size: u64,
    sha256: String,
    content_type: String,
    compressed: bool,
}

/// One version of a file's content, ready to read.
#[derive(Debug, Clone)]
pub struct FileContent {
    pub id: Ulid,
    pub name: String,
    pub content_type: String,
    pub size: u64,
    pub sha256: String,
    pub version: u64,
    pub modified_at: Option<String>,
    pub manifest: Manifest,
}

/// Files waiting for metadata extraction: (tessellation, ID, force).
pub struct ExtractQueue {
    tx: mpsc::UnboundedSender<(String, Ulid, bool)>,
    rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<(String, Ulid, bool)>>>,
}

impl Default for ExtractQueue {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        ExtractQueue { tx, rx: std::sync::Mutex::new(Some(rx)) }
    }
}

/// Copying file content from other hexes: manifests to fetch, counters, and
/// a client for hex-to-hex requests.
pub struct FileSync {
    tx: mpsc::UnboundedSender<ChunkId>,
    rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<ChunkId>>>,
    scan: tokio::sync::Notify,
    client: std::sync::OnceLock<Option<reqwest::Client>>,
    pub(crate) fetched: std::sync::atomic::AtomicU64,
    pub(crate) failed: std::sync::atomic::AtomicU64,
}

impl Default for FileSync {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        FileSync {
            tx,
            rx: std::sync::Mutex::new(Some(rx)),
            scan: tokio::sync::Notify::new(),
            client: std::sync::OnceLock::new(),
            fetched: Default::default(),
            failed: Default::default(),
        }
    }
}

/// Aad that binds a sealed chunk to its ID.
const CONTENT_KEY_AAD: &[u8] = b"HexDB content key";

/// A file's content read from the chunk store, for extraction.
struct BlobSource {
    store: Arc<blobs::BlobStore>,
    /// (chunk, offset of its first byte, length)
    chunks: Vec<(ChunkId, u64, u32)>,
    size: u64,
    current: Option<(usize, Arc<Vec<u8>>)>,
}

impl BlobSource {
    fn new(store: Arc<blobs::BlobStore>, manifest: &Manifest) -> BlobSource {
        let mut offset = 0;
        let chunks = manifest
            .chunks
            .iter()
            .map(|(id, len)| {
                let entry = (*id, offset, *len);
                offset += *len as u64;
                entry
            })
            .collect();
        BlobSource { store, chunks, size: offset, current: None }
    }
}

impl crate::extract::ByteSource for BlobSource {
    fn len(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        if offset >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let index = self.chunks.partition_point(|(_, start, _)| *start <= offset) - 1;
        let data = match &self.current {
            Some((i, data)) if *i == index => data.clone(),
            _ => {
                let data = self.store.get(&self.chunks[index].0).map_err(|e| std::io::Error::other(format!("{:#}", e)))?;
                self.current = Some((index, data.clone()));
                data
            }
        };
        let within = (offset - self.chunks[index].1) as usize;
        let n = buf.len().min(data.len() - within);
        buf[..n].copy_from_slice(&data[within..within + n]);
        Ok(n)
    }
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    EngineError::Invalid(message.into()).into()
}

fn not_found(message: impl Into<String>) -> anyhow::Error {
    EngineError::NotFound(message.into()).into()
}

fn now_text() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn time_text(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn caller_login() -> String {
    crate::access::caller().map(|p| p.login.clone()).unwrap_or_else(|| "system".into())
}

fn as_map(doc: &Document) -> Map<String, Value> {
    match doc.data_json() {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn document(tess: &str, id: Ulid, map: Map<String, Value>, ttl: Option<i64>) -> Document {
    Document { id, tessellation: tess.to_string(), data: infer_fields_from_json(&Value::Object(map)), ttl }
}

/// The manifest chunks a stored file document references.
pub(crate) fn manifests_of_doc(doc: &Document) -> Vec<ChunkId> {
    manifests_of(&as_map(doc))
}

/// The manifest chunks a file document references (current and earlier versions).
fn manifests_of(map: &Map<String, Value>) -> Vec<ChunkId> {
    let current = map.get("content");
    let earlier = map.get("versions").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.get("content"));
    current
        .into_iter()
        .chain(earlier)
        .filter_map(|c| c.get("manifest").and_then(Value::as_str).and_then(blobs::parse_chunk_id))
        .collect()
}

/// The fields describing one version's content, for `versions`.
fn version_entry(map: &Map<String, Value>) -> Value {
    let mut entry = Map::new();
    for key in ["version", "size", "content_type", "sha256", "content", "modified_at", "uploaded_at", "uploaded_by"] {
        if let Some(value) = map.get(key) {
            entry.insert(key.into(), value.clone());
        }
    }
    Value::Object(entry)
}

impl HexDBEngine {
    /// The chunk store.
    pub fn blobs(&self) -> &Arc<blobs::BlobStore> {
        &self.blobs
    }

    /// Fail unless `tess` is a files tessellation.
    pub fn files_tessellation(&self, tess: &str) -> Result<TessellationInfo> {
        match self.tessellation_info(tess) {
            Some(info) if info.is_files() => Ok(info),
            Some(_) => Err(invalid(format!("'{}' isn't a files tessellation. Create one with {{\"name\": ..., \"kind\": \"files\"}}.", tess))),
            None => Err(not_found(format!("Tessellation '{}' not found.", tess))),
        }
    }

    /// Create a files tessellation with its (path, name) index. False if it already exists.
    pub async fn create_files_tessellation(&self, name: &str) -> Result<bool> {
        self.ensure_writable()?;
        if let Some(info) = self.tessellation_info(name) {
            if !info.is_files() {
                return Err(EngineError::Conflict(format!("Tessellation '{}' already exists and holds documents, not files.", name)).into());
            }
            return Ok(false);
        }
        self.create_tessellation_unchecked(name, FILES_KIND)?;
        self.ensure_file_indexes(name).await?;
        Ok(true)
    }

    /// Create the indexes every files tessellation has, if missing.
    pub(crate) async fn ensure_file_indexes(&self, name: &str) -> Result<()> {
        let info = self.files_tessellation(name)?;
        if !info.indexes.iter().any(|i| i.name == PATH_INDEX) {
            let def = IndexDef { name: PATH_INDEX.into(), kind: IndexKind::Field, fields: vec!["path".into(), "name".into()], unique: true, analyzer: None };
            self.create_index_unchecked(name, def).await?;
        }
        // Every string in a file's name, folders, tags, description and
        // extracted metadata is searchable with $text.
        let fields = ["name", "path", "tags", "description", "meta", "ai"].map(String::from).to_vec();
        // Before 1.6 the search index didn't cover `ai` (AI tags and descriptions).
        let old = ["name", "path", "tags", "description", "meta"].map(String::from).to_vec();
        if let Some(existing) = info.indexes.iter().find(|i| i.name == files::SEARCH_INDEX && i.kind == IndexKind::Text && i.fields == old) {
            info!("🔎 Adding AI tags to the search index of '{}'.", name);
            self.drop_index_unchecked(name, &existing.name)?;
            let def = IndexDef { name: files::SEARCH_INDEX.into(), kind: IndexKind::Text, fields: fields.clone(), unique: false, analyzer: existing.analyzer.clone() };
            self.create_index_unchecked(name, def).await?;
        } else if !info.indexes.iter().any(|i| i.kind == IndexKind::Text) {
            let def = IndexDef { name: files::SEARCH_INDEX.into(), kind: IndexKind::Text, fields, unique: false, analyzer: None };
            self.create_index_unchecked(name, def).await?;
        }
        Ok(())
    }

    /// The live file at a path, with its version.
    async fn find_file(&self, tess: &str, path: &str, name: &str) -> Result<Option<(Document, u64)>> {
        let filter = Filter::parse(&json!({ "path": path, "name": name }))?;
        let Some(doc) = self.matching_documents(tess, &filter).await?.into_iter().next() else { return Ok(None) };
        let (doc, seq) = self.read_latest(&DocKey::new(tess, doc.id)).await?;
        Ok(doc.map(|d| (d, seq)))
    }

    /// Store a file at `file_path` (`folder/name`) in a files tessellation,
    /// streaming its content from `body`. A file already at that path gets a
    /// new version.
    pub async fn store_file<S, B, E>(&self, tess: &str, file_path: &str, options: UploadOptions, mut body: S) -> Result<UploadResult>
    where
        S: Stream<Item = std::result::Result<B, E>> + Unpin,
        B: AsRef<[u8]>,
        E: std::fmt::Display,
    {
        self.files_tessellation(tess)?;
        self.ensure_writable()?;
        let (path, name) = files::split_file_path(file_path)?;
        self.check_file_space(0)?;
        let max = self.config.files.max_file_mb.saturating_mul(1024 * 1024);

        let mut pins = Pins::new(self.blobs.clone());
        let mut chunker = Chunker::default();
        let mut sha = Sha256::new();
        let mut size = 0u64;
        let mut head = Vec::new();
        let mut manifest = Manifest::default();
        let mut content_type: Option<String> = None;
        let mut compressed = false;
        loop {
            let (ready, done) = match body.next().await {
                Some(Ok(bytes)) => {
                    let bytes = bytes.as_ref();
                    size += bytes.len() as u64;
                    if max > 0 && size > max {
                        return Err(EngineError::TooLarge(format!(
                            "The file is larger than {} MB (files.max_file_mb).",
                            self.config.files.max_file_mb
                        ))
                        .into());
                    }
                    sha.update(bytes);
                    if head.len() < 8192 {
                        head.extend_from_slice(&bytes[..bytes.len().min(8192 - head.len())]);
                    }
                    (chunker.push(bytes), false)
                }
                Some(Err(e)) => return Err(invalid(format!("The upload was interrupted: {}", e))),
                None => (std::mem::take(&mut chunker).finish(), true),
            };
            if !ready.is_empty() {
                // The first chunk is ready only with enough of the file to sniff.
                let kind = content_type.get_or_insert_with(|| files::sniff_type(&head, &name, options.content_type.as_deref()));
                let compress = files::should_compress(kind);
                for chunk in ready {
                    let len = chunk.len() as u32;
                    let (back, stored) = self.put_chunk(pins, chunk, compress).await?;
                    pins = back;
                    compressed |= compress;
                    manifest.chunks.push((stored.id, len));
                }
            }
            if done {
                break;
            }
        }
        let content_type = content_type.unwrap_or_else(|| files::sniff_type(&head, &name, options.content_type.as_deref()));
        let staged = Staged { manifest, size, sha256: hex::encode(sha.finalize()), content_type, compressed };
        self.commit_file(tess, &path, &name, &options, staged, pins).await
    }

    /// Store a file's manifest and make its chunks durable, then create the
    /// file at `path` + `name`, or add a version to the file already there.
    async fn commit_file(&self, tess: &str, path: &str, name: &str, options: &UploadOptions, staged: Staged, pins: Pins) -> Result<UploadResult> {
        let Staged { manifest, size, sha256, content_type, compressed } = staged;
        let (pins, stored) = self.put_chunk(pins, manifest.encode(), true).await?;
        let store = self.blobs.clone();
        tokio::task::spawn_blocking(move || store.sync()).await.map_err(|e| anyhow!("{}", e))??;
        let content = json!({ "manifest": blobs::chunk_hex(&stored.id), "chunks": manifest.chunks.len(), "compressed": compressed });

        let (read, write) = (self.caller_scope(tess, Action::Read), self.caller_scope(tess, Action::Upload));
        let login = caller_login();
        // Quotas (a new version counts its size; a new file counts one file too).
        let existing = self.find_file(tess, path, name).await?.is_some();
        self.check_quota(&login, size, !existing).await?;
        for _ in 0..MAX_WRITE_RETRIES {
            let now = now_text();
            let modified = options.modified_at.map(time_text).unwrap_or_else(|| now.clone());
            let (doc, expected, created) = match self.find_file(tess, path, name).await? {
                Some((previous, seq)) => {
                    if !write.allows(&previous) {
                        return Err(EngineError::Conflict(format!(
                            "A file named '{}' exists in {} outside what your role may change.",
                            name, path
                        ))
                        .into());
                    }
                    let mut map = as_map(&previous);
                    if map.get("sha256").and_then(Value::as_str) == Some(sha256.as_str()) && map.get("size").and_then(Value::as_u64) == Some(size) {
                        return Ok(UploadResult { document: read.mask(previous), created: false, unchanged: true });
                    }
                    let version = map.get("version").and_then(Value::as_u64).unwrap_or(1) + 1;
                    let mut versions = vec![version_entry(&map)];
                    versions.extend(map.get("versions").and_then(Value::as_array).cloned().unwrap_or_default());
                    map.insert("versions".into(), Value::Array(self.trim_versions(versions)));
                    map.insert("version".into(), json!(version));
                    map.insert("size".into(), json!(size));
                    map.insert("content_type".into(), json!(content_type));
                    map.insert("sha256".into(), json!(sha256));
                    map.insert("content".into(), content.clone());
                    map.insert("modified_at".into(), json!(modified));
                    map.insert("uploaded_at".into(), json!(now));
                    map.insert("uploaded_by".into(), json!(login));
                    map.remove("meta");
                    map.remove("thumbnail");
                    map.remove("ai");
                    map.insert("extract".into(), json!({ "status": "pending" }));
                    (document(tess, previous.id, map, previous.ttl), seq, false)
                }
                None => {
                    let created_at = options.created_at.map(time_text).unwrap_or_else(|| modified.clone());
                    let map = json!({
                        "name": name, "path": path, "size": size, "content_type": content_type, "sha256": sha256,
                        "content": content, "version": 1, "versions": [],
                        "created_at": created_at, "modified_at": modified, "uploaded_at": now, "uploaded_by": login,
                        "extract": { "status": "pending" },
                    });
                    let Value::Object(map) = map else { unreachable!() };
                    (document(tess, Ulid::new(), map, None), 0, true)
                }
            };
            if !write.allows(&doc) {
                return Err(EngineError::Forbidden(format!("Your role may not store this file in '{}'.", tess)).into());
            }
            let mut items = vec![BatchItem { key: DocKey::new(tess, doc.id), op: WalOp::Put(doc), expected_seq: Some(expected) }];
            self.run_before_triggers(&mut items).await?;
            let stored_doc = match &items[0].op {
                WalOp::Put(d) => d.clone(),
                _ => unreachable!(),
            };
            match files::as_internal(self.commit(items)).await {
                Ok(true) => {
                    drop(pins);
                    self.queue_extraction(tess, stored_doc.id, false);
                    self.quota_add(&login, size, created);
                return Ok(UploadResult { document: read.mask(stored_doc), created, unchanged: false });
                }
                Ok(false) => continue,
                // Another upload created the same path first: become a new version of it.
                Err(e) if created && matches!(e.downcast_ref::<EngineError>(), Some(EngineError::Conflict(_))) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(EngineError::Conflict("The file is being changed concurrently; try again.".into()).into())
    }

    // -----------------------------------------------------------------------
    // Resumable uploads
    // -----------------------------------------------------------------------

    /// An upload session the caller started.
    async fn upload_session(&self, tess: &str, upload_id: &str) -> Result<(Document, u64, Map<String, Value>)> {
        self.files_tessellation(tess)?;
        let missing = || not_found(format!("Upload {} not found (it may have expired).", upload_id));
        let id = Ulid::from_string(upload_id).map_err(|_| missing())?;
        if !self.tessellation_exists(UPLOADS_TESSELLATION) {
            return Err(missing());
        }
        let (doc, seq) = self.read_latest(&DocKey::new(UPLOADS_TESSELLATION, id)).await?;
        let doc = doc.ok_or_else(missing)?;
        let map = as_map(&doc);
        let mine = map.get("by").and_then(Value::as_str) == Some(caller_login().as_str());
        let admin = crate::access::caller().is_some_and(|p| p.is_admin());
        if map.get("tessellation").and_then(Value::as_str) != Some(tess) || !(mine || admin) {
            return Err(missing());
        }
        Ok((doc, seq, map))
    }

    fn session_view(id: Ulid, map: &Map<String, Value>, ttl: Option<i64>) -> Value {
        json!({
            "id": id.to_string(),
            "path": format!("{}{}", map.get("path").and_then(Value::as_str).unwrap_or("/"), map.get("name").and_then(Value::as_str).unwrap_or("")),
            "offset": map.get("offset").cloned().unwrap_or(json!(0)),
            "size": map.get("size").cloned().unwrap_or(Value::Null),
            "expires_at": ttl.and_then(DateTime::from_timestamp_millis).map(time_text),
        })
    }

    /// Start a resumable upload to `file_path`. `size`, if given, is checked at completion.
    pub async fn begin_upload(&self, tess: &str, file_path: &str, options: UploadOptions, size: Option<u64>) -> Result<Value> {
        self.files_tessellation(tess)?;
        self.ensure_writable()?;
        let (path, name) = files::split_file_path(file_path)?;
        let max = self.config.files.max_file_mb.saturating_mul(1024 * 1024);
        if let Some(size) = size.filter(|s| max > 0 && *s > max) {
            return Err(EngineError::TooLarge(format!("The file is {} MB; the limit is {} MB (files.max_file_mb).", size / (1024 * 1024), self.config.files.max_file_mb)).into());
        }
        self.check_file_space(size.unwrap_or(0))?;
        if let Some(size) = size {
            self.check_quota(&caller_login(), size, self.find_file(tess, &path, &name).await?.is_none()).await?;
        }
        self.disk_used.fetch_sub(size.unwrap_or(0), Ordering::Relaxed);
        let id = Ulid::new();
        let map = json!({
            "tessellation": tess, "path": path, "name": name, "size": size, "offset": 0,
            "declared_type": options.content_type, "modified_at": options.modified_at.map(time_text), "created_at": options.created_at.map(time_text),
            "by": caller_login(), "started_at": now_text(),
        });
        let Value::Object(map) = map else { unreachable!() };
        let ttl = Utc::now().timestamp_millis() + 24 * 3_600_000;
        self.ensure_system_tessellation(UPLOADS_TESSELLATION)?;
        let doc = document(UPLOADS_TESSELLATION, id, map.clone(), Some(ttl));
        let items = vec![BatchItem { key: DocKey::new(UPLOADS_TESSELLATION, id), op: WalOp::Put(doc), expected_seq: Some(0) }];
        files::as_internal(self.commit(items)).await?;
        Ok(Self::session_view(id, &map, Some(ttl)))
    }

    /// An upload session's state (how much has arrived).
    pub async fn upload_status(&self, tess: &str, upload_id: &str) -> Result<Value> {
        let (doc, _, map) = self.upload_session(tess, upload_id).await?;
        Ok(Self::session_view(doc.id, &map, doc.ttl))
    }

    /// Add bytes to an upload at `offset`, which must be where the last part ended.
    pub async fn upload_part<S, B, E>(&self, tess: &str, upload_id: &str, offset: u64, mut body: S) -> Result<Value>
    where
        S: Stream<Item = std::result::Result<B, E>> + Unpin,
        B: AsRef<[u8]>,
        E: std::fmt::Display,
    {
        self.ensure_writable()?;
        let (session, seq, mut map) = self.upload_session(tess, upload_id).await?;
        let current = map.get("offset").and_then(Value::as_u64).unwrap_or(0);
        if offset != current {
            return Err(EngineError::Conflict(format!("The upload is at byte {}; send the part that starts there.", current)).into());
        }
        let mut manifest = match map.get("content").and_then(|c| c.get("manifest")).and_then(Value::as_str).and_then(blobs::parse_chunk_id) {
            Some(id) => Manifest::decode(&self.read_file_chunk(id).await?)?,
            None => Manifest::default(),
        };
        let name = map.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
        let declared = map.get("declared_type").and_then(Value::as_str).map(String::from);
        let mut content_type = map.get("content_type").and_then(Value::as_str).map(String::from);
        let max = self.config.files.max_file_mb.saturating_mul(1024 * 1024);
        let expected = map.get("size").and_then(Value::as_u64);
        let mut pins = Pins::new(self.blobs.clone());
        let mut chunker = Chunker::default();
        let mut head = Vec::new();
        let mut size = current;
        loop {
            let (ready, done) = match body.next().await {
                Some(Ok(bytes)) => {
                    let bytes = bytes.as_ref();
                    size += bytes.len() as u64;
                    if (max > 0 && size > max) || expected.is_some_and(|e| size > e) {
                        return Err(EngineError::TooLarge("The upload is larger than its declared size or files.max_file_mb.".into()).into());
                    }
                    if head.len() < 8192 {
                        head.extend_from_slice(&bytes[..bytes.len().min(8192 - head.len())]);
                    }
                    (chunker.push(bytes), false)
                }
                Some(Err(e)) => return Err(invalid(format!("The part was interrupted: {}", e))),
                // Each part ends a chunk, so a session is just its chunks.
                None => (std::mem::take(&mut chunker).finish(), true),
            };
            if !ready.is_empty() {
                let kind = content_type.get_or_insert_with(|| files::sniff_type(&head, &name, declared.as_deref())).clone();
                let compress = files::should_compress(&kind);
                for chunk in ready {
                    let len = chunk.len() as u32;
                    let (back, stored) = self.put_chunk(pins, chunk, compress).await?;
                    pins = back;
                    manifest.chunks.push((stored.id, len));
                }
            }
            if done {
                break;
            }
        }
        let (pins, stored) = self.put_chunk(pins, manifest.encode(), true).await?;
        let store = self.blobs.clone();
        tokio::task::spawn_blocking(move || store.sync()).await.map_err(|e| anyhow!("{}", e))??;
        map.insert("offset".into(), json!(size));
        map.insert("content".into(), json!({ "manifest": blobs::chunk_hex(&stored.id) }));
        if let Some(kind) = &content_type {
            map.insert("content_type".into(), json!(kind));
        }
        let doc = document(UPLOADS_TESSELLATION, session.id, map.clone(), session.ttl);
        let items = vec![BatchItem { key: DocKey::new(UPLOADS_TESSELLATION, session.id), op: WalOp::Put(doc), expected_seq: Some(seq) }];
        if !files::as_internal(self.commit(items)).await? {
            return Err(EngineError::Conflict("Another part of this upload arrived at the same time; check the offset and resend.".into()).into());
        }
        drop(pins);
        Ok(Self::session_view(session.id, &map, session.ttl))
    }

    /// Finish an upload: the file is created (or gets a new version).
    pub async fn complete_upload(&self, tess: &str, upload_id: &str) -> Result<UploadResult> {
        self.ensure_writable()?;
        let (session, seq, map) = self.upload_session(tess, upload_id).await?;
        let size = map.get("offset").and_then(Value::as_u64).unwrap_or(0);
        if let Some(expected) = map.get("size").and_then(Value::as_u64) {
            if expected != size {
                return Err(invalid(format!("The upload has {} of {} bytes.", size, expected)));
            }
        }
        let manifest = match map.get("content").and_then(|c| c.get("manifest")).and_then(Value::as_str).and_then(blobs::parse_chunk_id) {
            Some(id) => Manifest::decode(&self.read_file_chunk(id).await?)?,
            None => Manifest::default(),
        };
        // The hash covers the whole file, read back in order.
        let mut pins = Pins::new(self.blobs.clone());
        let mut sha = Sha256::new();
        let mut head = Vec::new();
        let mut compressed = false;
        for (id, _) in &manifest.chunks {
            let data = self.read_file_chunk(*id).await?;
            sha.update(data.as_slice());
            if head.len() < 8192 {
                head.extend_from_slice(&data[..data.len().min(8192 - head.len())]);
            }
            compressed |= self.blobs.is_compressed(id).unwrap_or(false);
        }
        // Keep the chunks pinned until the file's document references them.
        let store = self.blobs.clone();
        let ids: Vec<ChunkId> = manifest.chunks.iter().map(|(id, _)| *id).collect();
        pins = tokio::task::spawn_blocking(move || {
            for id in ids {
                store.pin_existing(&id, &mut pins);
            }
            pins
        })
        .await
        .map_err(|e| anyhow!("{}", e))?;
        let name = map.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
        let path = map.get("path").and_then(Value::as_str).unwrap_or("/").to_string();
        let declared = map.get("declared_type").and_then(Value::as_str).map(String::from);
        let content_type = map.get("content_type").and_then(Value::as_str).map(String::from).unwrap_or_else(|| files::sniff_type(&head, &name, declared.as_deref()));
        let parse = |k: &str| map.get(k).and_then(Value::as_str).and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Utc));
        let options = UploadOptions { content_type: declared.clone(), modified_at: parse("modified_at"), created_at: parse("created_at") };
        let staged = Staged { manifest, size, sha256: hex::encode(sha.finalize()), content_type, compressed };
        let result = self.commit_file(tess, &path, &name, &options, staged, pins).await?;
        let items = vec![BatchItem {
            key: DocKey::new(UPLOADS_TESSELLATION, session.id),
            op: WalOp::Delete { tessellation: UPLOADS_TESSELLATION.into(), id: session.id },
            expected_seq: Some(seq),
        }];
        let _ = files::as_internal(self.commit(items)).await;
        Ok(result)
    }

    /// Abandon an upload; its chunks are freed by garbage collection.
    pub async fn abort_upload(&self, tess: &str, upload_id: &str) -> Result<()> {
        self.ensure_writable()?;
        let (session, _, _) = self.upload_session(tess, upload_id).await?;
        let items = vec![BatchItem {
            key: DocKey::new(UPLOADS_TESSELLATION, session.id),
            op: WalOp::Delete { tessellation: UPLOADS_TESSELLATION.into(), id: session.id },
            expected_seq: None,
        }];
        files::as_internal(self.commit(items)).await?;
        Ok(())
    }

    /// Store one chunk on the blocking pool, keeping the disk limit.
    async fn put_chunk(&self, mut pins: Pins, chunk: Vec<u8>, compress: bool) -> Result<(Pins, blobs::Stored)> {
        let store = self.blobs.clone();
        let (pins, stored) = tokio::task::spawn_blocking(move || {
            let stored = store.put(&chunk, compress, &mut pins);
            (pins, stored)
        })
        .await
        .map_err(|e| anyhow!("{}", e))?;
        let stored = stored?;
        self.check_file_space(stored.bytes)?;
        Ok((pins, stored))
    }

    /// Count `added` bytes against `storage.disk_mb` and refuse when over it.
    fn check_file_space(&self, added: u64) -> Result<()> {
        let used = self.disk_used.fetch_add(added, Ordering::Relaxed) + added;
        let limit = self.live().disk_mb.saturating_mul(1024 * 1024);
        if used > limit {
            return Err(EngineError::DiskFull(format!(
                "The data directory uses {} MB, over its {} MB limit (storage.disk_mb). Delete files, or raise the limit on the Settings page.",
                used / (1024 * 1024),
                limit / (1024 * 1024)
            ))
            .into());
        }
        Ok(())
    }

    /// Keep at most `files.max_versions` earlier versions, and none older than `files.version_days`.
    fn trim_versions(&self, mut versions: Vec<Value>) -> Vec<Value> {
        versions.truncate(self.config.files.max_versions);
        if self.config.files.version_days > 0 {
            let cutoff = Utc::now() - chrono::Duration::days(self.config.files.version_days as i64);
            versions.retain(|v| {
                v.get("uploaded_at")
                    .and_then(Value::as_str)
                    .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                    .is_none_or(|t| t.with_timezone(&Utc) >= cutoff)
            });
        }
        versions
    }

    /// A file's document, if the caller can see it.
    pub(crate) async fn visible_file(&self, tess: &str, id: &str) -> Result<(Document, u64)> {
        self.files_tessellation(tess)?;
        let missing = || not_found(format!("File {} not found in '{}'.", id, tess));
        let id = Ulid::from_string(id).map_err(|_| missing())?;
        let (doc, seq) = self.read_latest(&DocKey::new(tess, id)).await?;
        let doc = doc.filter(|d| self.caller_scope(tess, Action::Read).allows(d)).ok_or_else(missing)?;
        Ok((doc, seq))
    }

    /// The file at a path (`folder/name`), if the caller can see it.
    pub async fn file_at_path(&self, tess: &str, file_path: &str) -> Result<Document> {
        self.files_tessellation(tess)?;
        let (path, name) = files::split_file_path(file_path)?;
        let read = self.caller_scope(tess, Action::Read);
        match self.find_file(tess, &path, &name).await? {
            Some((doc, _)) if read.allows(&doc) => Ok(read.mask(doc)),
            _ => Err(not_found(format!("No file at {}{} in '{}'.", path, name, tess))),
        }
    }

    /// A version of a file's content (the current one by default).
    pub async fn file_content(&self, tess: &str, id: &str, version: Option<u64>) -> Result<FileContent> {
        let (doc, _) = self.visible_file(tess, id).await?;
        let map = as_map(&doc);
        let current = map.get("version").and_then(Value::as_u64).unwrap_or(1);
        let entry = match version {
            None => Value::Object(map.clone()),
            Some(v) if v == current => Value::Object(map.clone()),
            Some(v) => map
                .get("versions")
                .and_then(Value::as_array)
                .and_then(|list| list.iter().find(|e| e.get("version").and_then(Value::as_u64) == Some(v)).cloned())
                .ok_or_else(|| not_found(format!("File {} has no version {} (it may have expired).", id, v)))?,
        };
        let manifest_id = entry
            .get("content")
            .and_then(|c| c.get("manifest"))
            .and_then(Value::as_str)
            .and_then(blobs::parse_chunk_id)
            .ok_or_else(|| anyhow!("file {} has no content reference", id))?;
        let manifest = Manifest::decode(&self.read_file_chunk(manifest_id).await?)?;
        Ok(FileContent {
            id: doc.id,
            name: map.get("name").and_then(Value::as_str).unwrap_or("file").to_string(),
            content_type: entry.get("content_type").and_then(Value::as_str).unwrap_or("application/octet-stream").to_string(),
            size: manifest.size(),
            sha256: entry.get("sha256").and_then(Value::as_str).unwrap_or_default().to_string(),
            version: entry.get("version").and_then(Value::as_u64).unwrap_or(current),
            modified_at: entry.get("modified_at").and_then(Value::as_str).map(String::from),
            manifest,
        })
    }

    /// Read one chunk of file content: from this hex's chunk store, or when
    /// it isn't here (not replicated yet) or can't be rebuilt, from another
    /// hex in the lattice, keeping a copy.
    pub async fn read_file_chunk(&self, id: ChunkId) -> Result<Arc<Vec<u8>>> {
        match self.read_local_chunk(id).await {
            Ok(data) => Ok(data),
            Err(local) => match self.fetch_chunk(id).await {
                Ok(data) => Ok(data),
                Err(remote) => Err(EngineError::Unavailable(format!(
                    "The file's content can't be read on this hex ({:#}) or fetched from another ({:#}).",
                    local, remote
                ))
                .into()),
            },
        }
    }

    /// A file's thumbnail: (bytes, content type, chunk ID for caching).
    pub async fn file_thumbnail(&self, tess: &str, id: &str) -> Result<(Arc<Vec<u8>>, String, String)> {
        let (doc, _) = self.visible_file(tess, id).await?;
        let map = as_map(&doc);
        let thumb = map.get("thumbnail").ok_or_else(|| not_found(format!("File {} has no thumbnail.", id)))?;
        let chunk = thumb.get("chunk").and_then(Value::as_str).and_then(blobs::parse_chunk_id).ok_or_else(|| anyhow!("bad thumbnail reference"))?;
        let content_type = thumb.get("content_type").and_then(Value::as_str).unwrap_or("image/jpeg").to_string();
        Ok((self.read_file_chunk(chunk).await?, content_type, blobs::chunk_hex(&chunk)))
    }

    /// Read one chunk from this hex's chunk store only.
    pub async fn read_local_chunk(&self, id: ChunkId) -> Result<Arc<Vec<u8>>> {
        let store = self.blobs.clone();
        tokio::task::spawn_blocking(move || store.get(&id)).await.map_err(|e| anyhow!("{}", e))?
    }

    // -----------------------------------------------------------------------
    // Content across the lattice
    // -----------------------------------------------------------------------

    /// The content key, sealed for other hexes (hex).
    pub(crate) fn sealed_content_key(&self) -> String {
        let key = self.catalog.lock().unwrap().content_key.clone();
        hex::decode(&key).map(|k| hex::encode(self.lattice_keys.seal(&k, CONTENT_KEY_AAD))).unwrap_or_default()
    }

    /// Use the Overseer's content key, so chunks have the same IDs on every hex.
    pub(crate) fn adopt_content_key(&self, sealed: &str) -> Result<()> {
        if sealed.is_empty() {
            return Ok(());
        }
        let key = hex::decode(sealed)
            .ok()
            .and_then(|s| self.lattice_keys.open(&s, CONTENT_KEY_AAD))
            .and_then(|k| <[u8; 32]>::try_from(k).ok())
            .ok_or_else(|| anyhow!("the Overseer's content key can't be opened with this hex's lattice key"))?;
        let mut catalog = self.catalog.lock().unwrap();
        if catalog.content_key == hex::encode(key) {
            return Ok(());
        }
        catalog.content_key = hex::encode(key);
        catalog.save(&self.storage_dir, &self.keys)?;
        drop(catalog);
        self.blobs.set_content_key(key);
        info!("🔑 Adopted the lattice's content key for file chunks.");
        Ok(())
    }

    /// Give a replicated tessellation the Overseer's kind (e.g. "files").
    pub(crate) fn set_replicated_kind(&self, name: &str, kind: &str) -> Result<()> {
        let mut catalog = self.catalog.lock().unwrap();
        if let Some(info) = catalog.tessellations.get_mut(name) {
            if info.kind != kind {
                info.kind = kind.to_string();
                catalog.save(&self.storage_dir, &self.keys)?;
            }
        }
        Ok(())
    }

    /// True for tessellations whose documents reference file content.
    pub(crate) fn holds_files(&self, tess: &str) -> bool {
        tess == TRASH_TESSELLATION || self.tessellation_info(tess).is_some_and(|i| i.is_files())
    }

    pub(crate) fn queue_chunk_sync(&self, manifest: ChunkId) {
        let _ = self.file_sync.tx.send(manifest);
    }

    /// Look for file content this hex is missing, soon.
    pub fn request_chunk_catch_up(&self) {
        self.file_sync.scan.notify_one();
    }

    fn lattice_http(&self) -> Option<reqwest::Client> {
        self.file_sync
            .client
            .get_or_init(|| match crate::replication::lattice_client(&self.config, Duration::from_secs(120)) {
                Ok(client) => Some(client),
                Err(e) => {
                    warn!("⚠️ Can't talk to other hexes for file content: {:#}", e);
                    None
                }
            })
            .clone()
    }

    /// Copy a chunk from another hex (the Overseer first) and keep it.
    pub(crate) async fn fetch_chunk(&self, id: ChunkId) -> Result<Arc<Vec<u8>>> {
        let peers: Vec<crate::network::discovery::PeerHex> = {
            let peers = self.peers.lock().await;
            let me = self.id.to_string();
            let mut list: Vec<_> = peers.iter().filter(|p| p.status == "active" && p.hex.id != me).map(|p| p.hex.clone()).collect();
            list.sort_by_key(|p| p.role != crate::network::discovery::ROLE_OVERSEER);
            list
        };
        if peers.is_empty() {
            return Err(anyhow!("no other hex is reachable"));
        }
        let client = self.lattice_http().ok_or_else(|| anyhow!("no client for other hexes"))?;
        let path = format!("/lattice/chunks/{}", blobs::chunk_hex(&id));
        let mut last = anyhow!("no hex has chunk {}", blobs::chunk_hex(&id));
        for peer in peers {
            let url = format!("{}{}", crate::replication::peer_base_url(&peer), path);
            match self.signed_get(&client, &url, &path).await {
                Ok(Some((compress, sealed))) => {
                    let Some(plain) = self.lattice_keys.open(&sealed, &id) else {
                        last = anyhow!("hex '{}' sent a chunk this hex can't open", peer.name);
                        continue;
                    };
                    let store = self.blobs.clone();
                    let kept = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                        let mut pins = Pins::new(store.clone());
                        store.put_copy(&id, &plain, compress, &mut pins)?;
                        store.sync()?;
                        Ok(plain)
                    })
                    .await
                    .map_err(|e| anyhow!("{}", e))?;
                    match kept {
                        Ok(plain) => {
                            self.file_sync.fetched.fetch_add(1, Ordering::Relaxed);
                            return Ok(Arc::new(plain));
                        }
                        Err(e) => last = e,
                    }
                }
                Ok(None) => {}
                Err(e) => last = e,
            }
        }
        self.file_sync.failed.fetch_add(1, Ordering::Relaxed);
        Err(last)
    }

    /// A signed GET to another hex: `Some((compressed, body))`, or `None` for 404.
    async fn signed_get(&self, client: &reqwest::Client, url: &str, path: &str) -> Result<Option<(bool, Vec<u8>)>> {
        let keys = &self.lattice_keys;
        for index in 0..keys.len() {
            let response = client
                .get(url)
                .header(crate::replication::LATTICE_SIGNATURE_HEADER, keys.sign_request_with(index, "GET", path, b""))
                .send()
                .await
                .map_err(|e| anyhow!("GET {}: {}", url, e))?;
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED && index + 1 < keys.len() {
                continue;
            }
            if status == reqwest::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if !status.is_success() {
                return Err(anyhow!("GET {} returned {}", url, status));
            }
            let compressed = response.headers().get("x-hexdb-compress").is_some_and(|v| v == "1");
            let body = response.bytes().await.map_err(|e| anyhow!("GET {}: {}", url, e))?;
            return Ok(Some((compressed, body.to_vec())));
        }
        Ok(None)
    }

    /// Make sure a file's chunks are all here.
    async fn sync_manifest(&self, manifest: ChunkId) -> Result<()> {
        let bytes = if self.blobs.contains(&manifest) { self.read_local_chunk(manifest).await? } else { self.fetch_chunk(manifest).await? };
        let missing: Vec<ChunkId> = Manifest::decode(&bytes)?.chunks.into_iter().map(|(id, _)| id).filter(|id| !self.blobs.contains(id)).collect();
        let results: Vec<Result<Arc<Vec<u8>>>> = futures::stream::iter(missing).map(|id| self.fetch_chunk(id)).buffer_unordered(4).collect().await;
        if let Some(e) = results.into_iter().find_map(Result::err) {
            return Err(e);
        }
        Ok(())
    }

    /// Queue every file whose content may be missing here.
    async fn scan_for_missing_content(&self) -> Result<usize> {
        let mut manifests = HashSet::new();
        for (tess, _) in self.tessellation_details().into_iter().filter(|(name, _)| self.holds_files(name)) {
            let mut after = None;
            loop {
                let page = self.list_documents(&tess, after, 1000).await?;
                for doc in &page.documents {
                    manifests.extend(manifests_of_doc(doc));
                }
                match page.next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
        }
        let mut queued = 0;
        for manifest in manifests {
            // A manifest that's here usually means its chunks are too; check them anyway (cheap).
            self.queue_chunk_sync(manifest);
            queued += 1;
        }
        Ok(queued)
    }

    /// Copy missing file content from other hexes: what replication brings,
    /// and a full check after a full sync and every 10 minutes.
    pub async fn run_file_sync(self: Arc<Self>, mut shutdown: watch::Receiver<()>) {
        let Some(mut rx) = self.file_sync.rx.lock().unwrap().take() else { return };
        let mut tick = tokio::time::interval(Duration::from_secs(600));
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = tick.tick() => {
                    if !self.is_writable() {
                        if let Err(e) = self.scan_for_missing_content().await {
                            debug!("Couldn't scan for missing file content: {:#}", e);
                        }
                    }
                }
                _ = self.file_sync.scan.notified() => {
                    if let Err(e) = self.scan_for_missing_content().await {
                        debug!("Couldn't scan for missing file content: {:#}", e);
                    }
                }
                job = rx.recv() => {
                    let Some(first) = job else { break };
                    let mut batch = HashSet::from([first]);
                    while let Ok(more) = rx.try_recv() {
                        batch.insert(more);
                        if batch.len() >= 256 {
                            break;
                        }
                    }
                    for manifest in batch {
                        if let Err(e) = self.sync_manifest(manifest).await {
                            debug!("Couldn't copy file content {}: {:#}", blobs::chunk_hex(&manifest), e);
                        }
                    }
                }
            }
        }
    }

    /// Chunk store statistics, with lattice copying counters.
    pub fn files_stats(&self) -> blobs::BlobStats {
        let mut stats = self.blobs.stats();
        stats.fetched_from_peers = self.file_sync.fetched.load(Ordering::Relaxed);
        stats.fetch_failures = self.file_sync.failed.load(Ordering::Relaxed);
        stats.parser_sandbox = crate::extract::sandbox::stats();
        stats
    }

    /// Make an earlier version current again. The version being replaced is
    /// kept as the newest earlier version, so a rollback can be undone.
    pub async fn restore_file_version(&self, tess: &str, id: &str, version: u64) -> Result<Document> {
        self.ensure_writable()?;
        let write = self.caller_scope(tess, Action::Restore);
        let read = self.caller_scope(tess, Action::Read);
        for _ in 0..MAX_WRITE_RETRIES {
            let (doc, seq) = self.visible_file(tess, id).await?;
            if !write.allows(&doc) {
                return Err(EngineError::Forbidden(format!("Your role may not restore versions of this file in '{}'.", tess)).into());
            }
            let mut map = as_map(&doc);
            let current = map.get("version").and_then(Value::as_u64).unwrap_or(1);
            if version == current {
                return Ok(read.mask(doc));
            }
            let earlier: Vec<Value> = map.get("versions").and_then(Value::as_array).cloned().unwrap_or_default();
            let entry = earlier
                .iter()
                .find(|e| e.get("version").and_then(Value::as_u64) == Some(version))
                .cloned()
                .ok_or_else(|| not_found(format!("File {} has no version {} (it may have expired).", id, version)))?;
            let mut versions = vec![version_entry(&map)];
            versions.extend(earlier);
            for key in ["size", "content_type", "sha256", "content", "modified_at"] {
                match entry.get(key) {
                    Some(value) => map.insert(key.into(), value.clone()),
                    None => map.remove(key),
                };
            }
            map.insert("version".into(), json!(current + 1));
            map.insert("versions".into(), Value::Array(self.trim_versions(versions)));
            map.insert("uploaded_at".into(), json!(now_text()));
            map.insert("uploaded_by".into(), json!(caller_login()));
            map.insert("restored_from".into(), json!(version));
            map.remove("meta");
            map.remove("thumbnail");
            map.remove("ai");
            map.insert("extract".into(), json!({ "status": "pending" }));
            let doc = document(tess, doc.id, map, doc.ttl);
            let items = vec![BatchItem { key: DocKey::new(tess, doc.id), op: WalOp::Put(doc.clone()), expected_seq: Some(seq) }];
            if files::as_internal(self.commit(items)).await? {
                self.queue_extraction(tess, doc.id, false);
                return Ok(read.mask(doc));
            }
        }
        Err(EngineError::Conflict("The file is being changed concurrently; try again.".into()).into())
    }

    /// Files in the trash that came from `tess` and the caller can see.
    pub async fn list_trash(&self, tess: &str) -> Result<Vec<Document>> {
        self.files_tessellation(tess)?;
        if !self.tessellation_exists(TRASH_TESSELLATION) {
            return Ok(Vec::new());
        }
        let read = self.caller_scope(tess, Action::Read);
        let filter = Filter::parse(&json!({ "from": tess }))?;
        Ok(self
            .matching_documents(TRASH_TESSELLATION, &filter)
            .await?
            .into_iter()
            .filter(|d| read.allows(&Document { tessellation: tess.to_string(), ..d.clone() }))
            .map(|d| read.mask(d))
            .collect())
    }

    /// A trashed file from `tess`, if the caller can see it and `action` it.
    async fn trashed_file(&self, tess: &str, id: &str, action: Action) -> Result<(Document, u64)> {
        self.files_tessellation(tess)?;
        let missing = || not_found(format!("File {} is not in the trash of '{}'.", id, tess));
        let id = Ulid::from_string(id).map_err(|_| missing())?;
        if !self.tessellation_exists(TRASH_TESSELLATION) {
            return Err(missing());
        }
        let (doc, seq) = self.read_latest(&DocKey::new(TRASH_TESSELLATION, id)).await?;
        let doc = doc.ok_or_else(missing)?;
        if as_map(&doc).get("from").and_then(Value::as_str) != Some(tess) {
            return Err(missing());
        }
        let as_file = Document { tessellation: tess.to_string(), ..doc.clone() };
        if !self.caller_scope(tess, Action::Read).allows(&as_file) {
            return Err(missing());
        }
        if !self.caller_scope(tess, action).allows(&as_file) {
            return Err(EngineError::Forbidden(format!("Your role may not {} this file in '{}'.", action.name(), tess)).into());
        }
        Ok((doc, seq))
    }

    /// Put a trashed file back, optionally under another name or folder.
    pub async fn restore_file(&self, tess: &str, id: &str, name: Option<&str>, path: Option<&str>) -> Result<Document> {
        self.ensure_writable()?;
        let (trashed, seq) = self.trashed_file(tess, id, Action::Restore).await?;
        let mut map = as_map(&trashed);
        for key in TRASH_FIELDS {
            map.remove(*key);
        }
        if let Some(name) = name {
            files::validate_name(name)?;
            map.insert("name".into(), json!(name));
        }
        if let Some(path) = path {
            map.insert("path".into(), json!(files::normalize_path(path)?));
        }
        let doc = document(tess, trashed.id, map, None);
        if !self.caller_scope(tess, Action::Restore).allows(&doc) {
            return Err(EngineError::Forbidden(format!("Your role may not store this file in '{}'.", tess)).into());
        }
        let items = vec![
            BatchItem { key: DocKey::new(TRASH_TESSELLATION, trashed.id), op: WalOp::Delete { tessellation: TRASH_TESSELLATION.into(), id: trashed.id }, expected_seq: Some(seq) },
            BatchItem { key: DocKey::new(tess, doc.id), op: WalOp::Put(doc.clone()), expected_seq: None },
        ];
        match files::as_internal(self.commit(items)).await {
            Ok(true) => Ok(self.caller_scope(tess, Action::Read).mask(doc)),
            Ok(false) => Err(EngineError::Conflict("The file changed while being restored; try again.".into()).into()),
            Err(e) if matches!(e.downcast_ref::<EngineError>(), Some(EngineError::Conflict(_))) => Err(EngineError::Conflict(format!(
                "A file with the same name already exists in that folder. Restore it under another name (?name=) or folder (?path=). ({})",
                e
            ))
            .into()),
            Err(e) => Err(e),
        }
    }

    /// Delete a trashed file for good. False if it isn't in the trash.
    pub async fn purge_file(&self, tess: &str, id: &str) -> Result<bool> {
        self.quota_forget();
        self.ensure_writable()?;
        let (trashed, seq) = match self.trashed_file(tess, id, Action::Delete).await {
            Ok(found) => found,
            Err(e) if matches!(e.downcast_ref::<EngineError>(), Some(EngineError::NotFound(_))) => return Ok(false),
            Err(e) => return Err(e),
        };
        let op = WalOp::Delete { tessellation: TRASH_TESSELLATION.into(), id: trashed.id };
        let items = vec![BatchItem { key: DocKey::new(TRASH_TESSELLATION, trashed.id), op, expected_seq: Some(seq) }];
        files::as_internal(self.commit(items)).await
    }

    /// Purge every file in `tess`'s trash that the caller may change. Returns how many.
    pub async fn empty_trash(&self, tess: &str) -> Result<usize> {
        self.quota_forget();
        let mut purged = 0;
        for doc in self.list_trash(tess).await? {
            if self.purge_file(tess, &doc.id.to_string()).await.unwrap_or(false) {
                purged += 1;
            }
        }
        Ok(purged)
    }

    /// Drop a files tessellation's trashed files (when the tessellation is deleted).
    pub(crate) async fn drop_trash_of(&self, tess: &str) -> Result<()> {
        if !self.tessellation_exists(TRASH_TESSELLATION) {
            return Ok(());
        }
        let filter = Filter::parse(&json!({ "from": tess }))?;
        let items: Vec<BatchItem> = self
            .matching_documents(TRASH_TESSELLATION, &filter)
            .await?
            .into_iter()
            .map(|d| BatchItem { key: DocKey::new(TRASH_TESSELLATION, d.id), op: WalOp::Delete { tessellation: TRASH_TESSELLATION.into(), id: d.id }, expected_seq: None })
            .collect();
        for batch in items.chunks(1000) {
            files::as_internal(self.commit(batch.to_vec())).await?;
        }
        Ok(())
    }

    /// Enforce the files rules on document writes made through the generic
    /// routes (see the module comment). Runs when a write is planned and again
    /// when it commits; the second pass changes nothing.
    pub(crate) async fn guard_file_writes(&self, items: &mut Vec<BatchItem>) -> Result<()> {
        if files::is_internal() {
            return Ok(());
        }
        let trashing: HashSet<Ulid> = items
            .iter()
            .filter(|i| i.key.tessellation == TRASH_TESSELLATION && matches!(i.op, WalOp::Put(_)))
            .map(|i| i.key.id)
            .collect();
        let mut added = Vec::new();
        for item in items.iter_mut() {
            let tess = item.key.tessellation.clone();
            if !self.tessellation_info(&tess).is_some_and(|i| i.is_files()) {
                continue;
            }
            match &mut item.op {
                WalOp::Put(doc) => {
                    let (previous, _) = self.read_latest(&item.key).await?;
                    let Some(previous) = previous else {
                        return Err(invalid(format!(
                            "Files are added by uploading their content: PUT /{}/_files/<folder>/<name>.",
                            tess
                        )));
                    };
                    restore_system_fields(doc, &previous)?;
                }
                WalOp::Delete { id, .. } => {
                    let id = *id;
                    if trashing.contains(&id) {
                        continue;
                    }
                    let (previous, _) = self.read_latest(&item.key).await?;
                    let Some(previous) = previous else { continue };
                    // Deleting a file needs `delete` (not only `write`), within the role's restrictions.
                    if let Some(principal) = crate::access::caller() {
                        if !principal.can(crate::auth::Permission::Delete, &tess) || !principal.scope(&tess, Action::Delete).allows(&previous) {
                            return Err(EngineError::Forbidden(format!("Your role may not delete this file in '{}'.", tess)).into());
                        }
                    }
                    let mut map = as_map(&previous);
                    map.insert("from".into(), json!(tess));
                    map.insert("trashed_at".into(), json!(now_text()));
                    map.insert("trashed_by".into(), json!(caller_login()));
                    let expires = Utc::now().timestamp_millis() + (self.config.files.trash_days.max(1) as i64) * 86_400_000;
                    let doc = document(TRASH_TESSELLATION, id, map, Some(expires));
                    added.push(BatchItem { key: DocKey::new(TRASH_TESSELLATION, id), op: WalOp::Put(doc), expected_seq: None });
                }
                WalOp::Batch(_) => {}
            }
        }
        if !added.is_empty() {
            self.ensure_system_tessellation(TRASH_TESSELLATION)?;
            items.extend(added);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Metadata extraction
    // -----------------------------------------------------------------------

    /// Queue a file for metadata extraction (`force`: even if it's up to date).
    pub(crate) fn queue_extraction(&self, tess: &str, id: Ulid, force: bool) {
        if self.config.files.extract {
            let _ = self.extract_queue.tx.send((tess.to_string(), id, force));
        }
    }

    /// Queue files for extraction again: one, or every file in the tessellation. Returns how many.
    pub async fn reextract_files(&self, tess: &str, id: Option<&str>) -> Result<usize> {
        self.ensure_writable()?;
        self.files_tessellation(tess)?;
        if !self.config.files.extract {
            return Err(invalid("Metadata extraction is turned off (files.extract = false)."));
        }
        if let Some(id) = id {
            let (doc, _) = self.visible_file(tess, id).await?;
            self.queue_extraction(tess, doc.id, true);
            return Ok(1);
        }
        let read = self.caller_scope(tess, Action::Read);
        let mut queued = 0;
        let mut after = None;
        loop {
            let page = self.list_documents(tess, after, 1000).await?;
            for doc in page.documents.iter().filter(|d| read.allows(d)) {
                self.queue_extraction(tess, doc.id, true);
                queued += 1;
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        Ok(queued)
    }

    /// True if a file's metadata is missing or older than its content or the extractors.
    fn needs_extraction(map: &Map<String, Value>) -> bool {
        let status = map.get("extract");
        let field = |k: &str| status.and_then(|s| s.get(k));
        field("status").and_then(Value::as_str) != Some("done")
            || field("version").and_then(Value::as_u64) != Some(crate::extract::VERSION)
            || field("sha256") != map.get("sha256")
    }

    /// Create missing file indexes and queue every file whose metadata is out
    /// of date. Run when this hex becomes the Overseer.
    async fn catch_up_extraction(&self) -> Result<usize> {
        let mut queued = 0;
        for (tess, info) in self.tessellation_details() {
            if !info.is_files() {
                continue;
            }
            if let Err(e) = self.ensure_file_indexes(&tess).await {
                warn!("⚠️ Couldn't create the indexes of '{}': {:#}", tess, e);
            }
            let mut after = None;
            loop {
                let page = self.list_documents(&tess, after, 1000).await?;
                for doc in &page.documents {
                    if Self::needs_extraction(&as_map(doc)) {
                        self.queue_extraction(&tess, doc.id, false);
                        queued += 1;
                    }
                }
                match page.next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
        }
        Ok(queued)
    }

    /// Extract metadata for queued files, one at a time, while this hex is the Overseer.
    pub async fn run_extraction(self: Arc<Self>, mut shutdown: watch::Receiver<()>) {
        let Some(mut rx) = self.extract_queue.rx.lock().unwrap().take() else { return };
        let mut caught_up = false;
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = tick.tick() => {
                    // Catch up whenever this hex becomes the Overseer.
                    let writable = self.is_writable() && self.config.files.extract;
                    if writable && !caught_up {
                        match self.catch_up_extraction().await {
                            Ok(n) if n > 0 => info!("🔎 Queued {} file(s) for metadata extraction.", n),
                            Ok(_) => {}
                            Err(e) => warn!("⚠️ Couldn't list files needing extraction: {:#}", e),
                        }
                    }
                    caught_up = writable;
                }
                job = rx.recv() => {
                    let Some((tess, id, force)) = job else { break };
                    if !self.is_writable() {
                        continue;
                    }
                    if let Err(e) = self.extract_file(&tess, id, force).await {
                        debug!("Metadata extraction for {}/{} stopped: {:#}", tess, id, e);
                    }
                }
            }
        }
    }

    /// Extract one file's metadata and store it on its document.
    async fn extract_file(&self, tess: &str, id: Ulid, force: bool) -> Result<()> {
        let key = DocKey::new(tess, id);
        let (Some(doc), _) = self.read_latest(&key).await? else { return Ok(()) };
        let map = as_map(&doc);
        if !force && !Self::needs_extraction(&map) {
            return Ok(());
        }
        let sha256 = map.get("sha256").cloned().unwrap_or(Value::Null);
        let name = map.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
        let content_type = map.get("content_type").and_then(Value::as_str).unwrap_or("application/octet-stream").to_string();
        let manifest_id = map
            .get("content")
            .and_then(|c| c.get("manifest"))
            .and_then(Value::as_str)
            .and_then(blobs::parse_chunk_id)
            .ok_or_else(|| anyhow!("no content reference"))?;
        let manifest = Manifest::decode(&self.read_file_chunk(manifest_id).await?)?;
        let limits = crate::extract::Limits {
            max_read_bytes: self.config.files.extract_max_mb.max(1) * 1024 * 1024,
            timeout: Duration::from_secs(self.config.files.extract_timeout_seconds.max(1)),
            max_text_bytes: (self.config.files.text_kb as usize) * 1024,
        };
        let store = self.blobs.clone();
        // A thumbnail the AI chose for this content is kept.
        let kept_thumbnail = Self::kept_ai_thumbnail(&map);
        let thumbnail_px = (kept_thumbnail.is_none() && self.config.files.thumbnails && crate::extract::thumbnail::supported(&content_type))
            .then_some(self.config.files.thumbnail_px.clamp(32, 2048));
        let file_type = content_type.clone();
        // Parsed in the sandbox worker when it's on (see crate::extract::sandbox).
        let work = tokio::task::spawn_blocking(move || {
            crate::extract::sandbox::extract(Box::new(BlobSource::new(store, &manifest)), &name, &content_type, &limits, thumbnail_px)
        });
        // The source stops reading at its deadline and the sandbox kills a stuck
        // worker; this is the last backstop (a stuck in-process parser).
        let (mut extracted, thumbnail) = match tokio::time::timeout(limits.timeout * 4 + Duration::from_secs(10), work).await {
            Ok(Ok(done)) => done,
            Ok(Err(e)) => (crate::extract::Extracted { errors: vec![format!("extraction failed: {}", e)], ..Default::default() }, None),
            Err(_) => (crate::extract::Extracted { errors: vec!["extraction took too long".into()], ..Default::default() }, None),
        };
        let mut pins = Pins::new(self.blobs.clone());
        let mut thumbnail_field = kept_thumbnail;
        match thumbnail {
            Some(Ok(Some(thumb))) => {
                let (back, stored) = self.put_chunk(pins, thumb.bytes, false).await?;
                pins = back;
                thumbnail_field = Some(json!({ "chunk": blobs::chunk_hex(&stored.id), "width": thumb.width, "height": thumb.height, "content_type": thumb.content_type }));
                let store = self.blobs.clone();
                tokio::task::spawn_blocking(move || store.sync()).await.map_err(|e| anyhow!("{}", e))??;
            }
            Some(Ok(None)) | None => {}
            Some(Err(e)) => extracted.errors.push(format!("thumbnail: {}", crate::extract::truncate_text(&format!("{:#}", e), 300))),
        }
        let mut status = extracted.status();
        status["status"] = json!("done");
        status["sha256"] = sha256.clone();
        if thumbnail_field.is_some() {
            if let Some(list) = status["extractors"].as_array_mut() {
                list.push(json!("thumbnail"));
            }
        }

        for _ in 0..MAX_WRITE_RETRIES {
            let (Some(current), seq) = self.read_latest(&key).await? else { return Ok(()) };
            let mut map = as_map(&current);
            if map.get("sha256") != Some(&sha256) {
                // New content arrived meanwhile; its own job will run.
                return Ok(());
            }
            map.insert("meta".into(), Value::Object(extracted.meta.clone()));
            map.insert("extract".into(), status.clone());
            match &thumbnail_field {
                Some(thumb) => map.insert("thumbnail".into(), thumb.clone()),
                None => map.remove("thumbnail"),
            };
            let doc = document(tess, id, map, current.ttl);
            let items = vec![BatchItem { key: key.clone(), op: WalOp::Put(doc), expected_seq: Some(seq) }];
            if files::as_internal(self.commit(items)).await? {
                drop(pins);
                self.queue_ai_after_extraction(tess, id, &file_type);
                return Ok(());
            }
        }
        Err(anyhow!("the file kept changing"))
    }

    /// Verify every shard in the chunk store and repair what can be repaired.
    pub async fn scrub_files(&self) -> Result<ScrubReport> {
        let store = self.blobs.clone();
        let (mut report, lost) = tokio::task::spawn_blocking(move || store.scrub()).await.map_err(|e| anyhow!("{}", e))?;
        // What can't be rebuilt here comes from another hex.
        for id in lost {
            if self.fetch_chunk(id).await.is_ok() {
                report.unrecoverable -= 1;
                report.repaired_from_peers += 1;
            }
        }
        if report.corrupt_shards > 0 {
            warn!(
                "🧽 Scrub: {} chunk(s), {} bad shard(s), {} repaired, {} unrecoverable.",
                report.chunks, report.corrupt_shards, report.repaired_shards, report.unrecoverable
            );
        }
        Ok(report)
    }

    /// Remove chunks no file document (current, earlier version or trashed) references.
    pub async fn collect_file_garbage(&self) -> Result<SweepReport> {
        let mark = self.blobs.begin_mark();
        let mut manifests = HashSet::new();
        let mut thumbnails = HashSet::new();
        let holders: Vec<String> = self
            .tessellation_details()
            .into_iter()
            .filter(|(name, info)| info.is_files() || name == TRASH_TESSELLATION || name == UPLOADS_TESSELLATION)
            .map(|(name, _)| name)
            .collect();
        for tess in holders {
            let mut after = None;
            loop {
                let page = self.list_documents(&tess, after, 1000).await?;
                for doc in &page.documents {
                    let map = as_map(doc);
                    manifests.extend(manifests_of(&map));
                    if let Some(thumb) = map.get("thumbnail").and_then(|t| t.get("chunk")).and_then(Value::as_str).and_then(blobs::parse_chunk_id) {
                        thumbnails.insert(thumb);
                    }
                }
                match page.next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
        }
        let mut live: HashSet<ChunkId> = thumbnails;
        for manifest in manifests {
            live.insert(manifest);
            if !self.blobs.contains(&manifest) {
                // Not on this hex (yet): nothing here to keep.
                continue;
            }
            let bytes = self.read_file_chunk(manifest).await.map_err(|e| anyhow!("Garbage collection stopped: {:#}", e))?;
            live.extend(Manifest::decode(&bytes)?.chunks.into_iter().map(|(id, _)| id));
        }
        let store = self.blobs.clone();
        let report = tokio::task::spawn_blocking(move || store.sweep(&live, mark)).await.map_err(|e| anyhow!("{}", e))??;
        if report.chunks_removed > 0 || report.packs_deleted > 0 || report.chunks_reencrypted > 0 {
            info!(
                "🧹 Files: removed {} unreferenced chunk(s), re-encrypted {}, rewrote {} pack(s), deleted {} ({} MB freed).",
                report.chunks_removed,
                report.chunks_reencrypted,
                report.packs_rewritten,
                report.packs_deleted,
                report.bytes_freed / (1024 * 1024)
            );
        }
        Ok(report)
    }
}

/// Keep a file document's system fields as stored, and check name and path.
fn restore_system_fields(doc: &mut Document, previous: &Document) -> Result<()> {
    for field in SYSTEM_FIELDS {
        match previous.data.get(*field) {
            Some(value) => doc.data.insert(field.to_string(), value.clone()),
            None => doc.data.remove(*field),
        };
    }
    let text = |d: &Document, key: &str| d.data.get(key).map(|v| v.to_json()).and_then(|v| v.as_str().map(String::from));
    let name = match text(doc, "name") {
        Some(name) => name,
        None if doc.data.contains_key("name") => return Err(invalid("A file's name must be a string.")),
        None => text(previous, "name").unwrap_or_default(),
    };
    files::validate_name(&name)?;
    let path = match text(doc, "path") {
        Some(path) => files::normalize_path(&path)?,
        None if doc.data.contains_key("path") => return Err(invalid("A file's path must be a string.")),
        None => text(previous, "path").unwrap_or_else(|| "/".into()),
    };
    doc.data.insert("name".into(), crate::document::FieldValue::from_json(&json!(name)));
    doc.data.insert("path".into(), crate::document::FieldValue::from_json(&json!(path)));
    Ok(())
}
