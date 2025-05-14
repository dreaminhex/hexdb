// HexDB Core SSTable Module
// This module implements the SSTable (Sorted String Table) format for HexDB.
// The SSTable format is used for storing large amounts of data in a compact
// and efficient manner. The module provides functions for writing and reading
// SSTable files, as well as for compressing and decompressing data using
// Zstandard (zstd) compression. The SSTable format is designed to be fast and
// efficient, allowing for quick access to data while minimizing disk space usage.

use crate::{document::Document, Hex, HexConfig, Wal};
use anyhow::{Context, Result};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use chrono::Utc;
use futures::future::try_join_all;
use serde_json;
use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::{self, BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs,
    sync::{mpsc::Sender, Mutex},
};
use tracing::{debug, error, info};
use ulid::Ulid;
use zstd::stream::{decode_all, encode_all};

const MAGIC: &[u8; 4] = b"HXDB";
const VERSION: u16 = 1;
const COMPRESSION_ZSTD: u8 = 1;

#[derive(Debug)]
pub struct SstEntry {
    pub id: Ulid,
    pub ttl: Option<i64>,
    pub data: Vec<u8>,
}

#[derive(Clone)]
pub struct SstWriter;

impl SstWriter {
    /// Write a list of entries to an SSTable file. The entries are compressed using Zstandard.
    /// The function creates a new file, writes the header, and then writes the entries.
    /// The header includes metadata such as the magic number, version, compression type,
    /// entry count, creation time, and index offset.
    pub async fn write_all(
        hex: Arc<Mutex<Hex>>,
        config: &HexConfig,
        wal_tx: &Sender<Wal>,
    ) -> Result<()> {
        let base = PathBuf::from(&config.storage.path);
        fs::create_dir_all(&base).await?;

        let tessellations: Vec<String> = {
            let hex = hex.lock().await;
            hex.tessellations.keys().cloned().collect()
        };

        let tasks = tessellations.into_iter().map(|tess| {
            let hex = Arc::clone(&hex);
            let config = config.clone();
            let base = base.clone();

            tokio::spawn(async move {
                let node = hex.lock().await;
                let entries = node.get_all_docs(&tess);
                drop(node);

                let now = Utc::now().timestamp_millis();

                let valid_docs: Vec<(Ulid, Document)> = entries
                    .into_iter()
                    .filter_map(|(id, data)| {
                        let doc: Document = serde_json::from_slice(&data[..]).ok()?;
                        if let Some(ttl) = doc.ttl {
                            if ttl < now {
                                return None;
                            }
                        }
                        Some((Ulid::from_string(&id).ok()?, doc))
                    })
                    .collect();

                if valid_docs.is_empty() {
                    return Result::<usize, anyhow::Error>::Ok(0);
                }

                let folder = base.join(&tess);
                fs::create_dir_all(&folder).await.ok();
                let file_path = folder.join(format!("{}.hxs", Ulid::new()));

                SstWriter::write(&config, &file_path, valid_docs.clone())
                    .with_context(|| format!("❌ Failed to write SSTable for {}", tess))?;

                Ok(valid_docs.len())
            })
        });

        let results: Vec<_> = try_join_all(tasks)
            .await?
            .into_iter()
            .filter_map(Result::ok)
            .collect();

        let total_written: usize = results.into_iter().sum();

        if total_written > 0 {
            wal_tx.send(Wal::Rotate).await.ok();
            info!("✅ Flushed {} documents to SSTables.", total_written);
        } else {
            info!("💾 No valid documents to flush.");
        }

        Ok(())
    }

    /// Private function to write a single SSTable file.
    /// It takes a path and a vector of entries, each containing an Ulid and a Document.
    fn write<P: AsRef<Path>>(
        config: &HexConfig,
        path: P,
        entries: Vec<(Ulid, Document)>,
    ) -> io::Result<()> {
        let mut file = BufWriter::new(File::create(path)?);
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let mut index = Vec::new();
        let mut entry_buf = Vec::new();
        let mut offset = 64u64;

        for (i, (id, doc)) in entries.iter().enumerate() {
            let mut flags = 0u8;
            if doc.ttl.is_some() {
                flags |= 0b00000001;
            }

            let raw_json = serde_json::to_vec(doc)?;
            let compressed = encode_all(&raw_json[..], config.compression.compression_level)?;
            let length = compressed.len() as u32;

            entry_buf.write_all(&id.to_bytes())?;
            entry_buf.write_u8(flags)?;
            if let Some(ttl_ms) = doc.ttl {
                entry_buf.write_i64::<BigEndian>(ttl_ms)?;
            }
            entry_buf.write_u32::<BigEndian>(length)?;
            entry_buf.write_all(&compressed)?;

            if i % 100 == 0 {
                index.push((
                    *id,
                    offset,
                    (16 + 1 + if doc.ttl.is_some() { 8 } else { 0 } + 4 + compressed.len()) as u32,
                ));
            }

            offset +=
                (16 + 1 + if doc.ttl.is_some() { 8 } else { 0 } + 4 + compressed.len()) as u64;
        }

        file.seek(SeekFrom::Start(64))?;
        file.write_all(&entry_buf)?;

        let index_offset = file.stream_position()?;
        for (ulid, off, len) in &index {
            file.write_all(&ulid.to_bytes())?;
            file.write_u64::<BigEndian>(*off)?;
            file.write_u32::<BigEndian>(*len)?;
        }
        let index_size = file.stream_position()? - index_offset;

        file.seek(SeekFrom::Start(0))?;
        file.write_all(MAGIC)?;
        file.write_u16::<BigEndian>(VERSION)?;
        file.write_u8(COMPRESSION_ZSTD)?;
        file.write_u8(0)?; // reserved
        file.write_u64::<BigEndian>(entries.len() as u64)?;
        file.write_i64::<BigEndian>(created)?;
        file.write_u64::<BigEndian>(index_offset)?;
        file.write_u64::<BigEndian>(index_size as u64)?;
        file.write_u64::<BigEndian>(0)?; // checksum placeholder
        file.write_all(&[0u8; 16])?;

        Ok(())
    }
}

#[derive(Clone)]
pub struct SstReader;

impl SstReader {
    /// Read all SSTable files from the disk and load them into the Hex.
    /// This function scans the `.hexdb` directory, finds all SSTable files,
    /// and loads their contents into the Hex.
    pub async fn read_all(hex: &Arc<Mutex<Hex>>) -> Result<()> {
        let base = PathBuf::from(".hexdb");
        if !base.exists() {
            return Ok(());
        }

        let mut dirs = fs::read_dir(&base).await?;
        let mut loaded = 0;

        while let Some(entry) = dirs.next_entry().await? {
            let path = entry.path();
            if path.is_dir() {
                if let Some(tess) = path.file_name().and_then(|n| n.to_str()) {
                    let mut files = fs::read_dir(&path).await?;
                    while let Some(file_entry) = files.next_entry().await? {
                        let file_path = file_entry.path();
                        if file_path.extension().map_or(false, |ext| ext == "hxs") {
                            let entries = Self::read(&file_path)?;
                            let mut node = hex.lock().await;
                            for (id, doc) in entries {
                                if let Some(ttl) = doc.ttl {
                                    if ttl < Utc::now().timestamp_millis() {
                                        continue;
                                    }
                                }
                                node.create_tessellation(tess, "user");
                                node.create_document(
                                    tess,
                                    &id.to_string(),
                                    &serde_json::to_vec(&doc)?,
                                );
                                loaded += 1;
                            }
                        }
                    }
                }
            }
        }

        info!("✅ Loaded {} documents from SSTables.", loaded);
        Ok(())
    }

    /// Reload the most recent SSTable per tessellation to rewarm hot documents into memory.
    pub async fn refresh_cache(hex: &Arc<Mutex<Hex>>, sst_base_path: &str) -> Result<()> {
        let base = PathBuf::from(sst_base_path);
        if !base.exists() {
            return Ok(());
        }

        let mut loaded = 0;
        let mut tess_dirs = fs::read_dir(&base).await?;

        while let Some(entry) = tess_dirs.next_entry().await? {
            let tess_path = entry.path();
            if !tess_path.is_dir() {
                continue;
            }

            let tess_name = match tess_path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name.to_string(),
                None => continue,
            };

            // Gather and sort SST files by modified time descending
            let mut sst_files = fs::read_dir(&tess_path).await?;
            let mut files_with_meta = Vec::new();

            while let Some(f) = sst_files.next_entry().await? {
                let path = f.path();
                if SstUtil::is_sstable(&path) {
                    let meta = fs::metadata(&path).await.ok();
                    let modified = meta.and_then(|m| m.modified().ok());
                    files_with_meta.push((path, modified));
                }
            }

            files_with_meta.sort_by_key(|(_, modified)| modified.map(std::cmp::Reverse));
            let files: Vec<_> = files_with_meta.into_iter().map(|(p, _)| p).collect();

            if let Some(latest) = files.first() {
                let map = SstReader::read(latest)?;
                let mut node = hex.lock().await;

                for (id, doc) in map {
                    if let Some(ttl) = doc.ttl {
                        if ttl < Utc::now().timestamp_millis() {
                            continue;
                        }
                    }

                    node.create_tessellation(&tess_name, "user");
                    node.create_document(&tess_name, &id.to_string(), &serde_json::to_vec(&doc)?);
                    loaded += 1;
                }
            }
        }

        info!(
            "🔥 Percolated {} documents from latest SSTables into memory.",
            loaded
        );
        Ok(())
    }

    /// Private function to read a single SSTable file.
    fn read<P: AsRef<Path>>(path: P) -> std::io::Result<BTreeMap<Ulid, Document>> {
        let mut file = BufReader::new(File::open(path)?);

        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "❌ Invalid SSTable magic header.",
            ));
        }

        let version = file.read_u16::<BigEndian>()?;
        let compression = file.read_u8()?;
        let _reserved = file.read_u8()?;
        let entry_count = file.read_u64::<BigEndian>()?;
        let _created = file.read_i64::<BigEndian>()?;
        let index_offset = file.read_u64::<BigEndian>()?;
        let _index_size = file.read_u64::<BigEndian>()?;
        let _checksum = file.read_u64::<BigEndian>()?;
        let mut _reserved2 = [0u8; 16];
        file.read_exact(&mut _reserved2)?;

        if version != VERSION || compression != COMPRESSION_ZSTD {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "❌ Unsupported SST version or compression.",
            ));
        }

        file.seek(SeekFrom::Start(64))?;

        let mut map = BTreeMap::new();
        for _ in 0..entry_count {
            if file.stream_position()? >= index_offset {
                break;
            }

            let mut id_buf = [0u8; 16];
            file.read_exact(&mut id_buf)?;
            let id = Ulid::from(id_buf);
            let flags = file.read_u8()?;
            let has_ttl = flags & 0b00000001 != 0;
            let ttl = if has_ttl {
                Some(file.read_i64::<BigEndian>()?)
            } else {
                None
            };

            let clen = file.read_u32::<BigEndian>()?;
            let mut comp = vec![0u8; clen as usize];
            file.read_exact(&mut comp)?;

            let json_bytes = decode_all(&comp[..])?;
            let mut doc: Document = serde_json::from_slice(&json_bytes)?;
            doc.ttl = ttl;

            map.insert(id, doc);
        }

        Ok(map)
    }
}

#[derive(Clone)]
pub struct SstUtil {
    pub config: HexConfig,
    pub total_doc_bytes: Arc<AtomicUsize>,
}

impl SstUtil {
    /// Create a new SstUtil instance with the given configuration.
    pub fn new(config: HexConfig) -> Self {
        SstUtil {
            config,
            total_doc_bytes: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Read all SSTable files from the disk and load them into the Hex.
    pub async fn read_all(&self, hex: &Arc<Mutex<Hex>>) -> Result<()> {
        SstReader::read_all(hex).await
    }

    /// Flush the Write-Ahead Log (WAL) to SSTables.
    pub async fn flush(&self, hex: Arc<Mutex<Hex>>, wal_tx: &Sender<Wal>) {
        SstWriter::write_all(hex, &self.config, wal_tx)
            .await
            .unwrap_or_else(|e| {
                error!("❌ Failed to flush WAL to SSTables: {}.", e);
            });
    }

    /// Compact all SSTable files in the storage directory.
    /// This function reads all SSTable files, filters out expired documents,
    /// and writes a new compacted SSTable file.
    /// It also deletes the obsolete SSTable files.
    pub async fn compact(&self) -> Result<()> {
        let base = PathBuf::from(&self.config.storage.path);
        if !base.exists() {
            return Ok(());
        }

        let mut dirs = fs::read_dir(&base).await?;
        while let Some(entry) = dirs.next_entry().await? {
            let path = entry.path();
            if path.is_dir() {
                if let Some(tess) = path.file_name().and_then(|n| n.to_str()) {
                    self.compact_tessellation(tess).await?;
                }
            }
        }

        Ok(())
    }

    /// Check if the given path is an SSTable file.
    /// This function checks the file extension to determine if it is a valid SSTable file.
    pub fn is_sstable(path: &Path) -> bool {
        path.extension().map_or(false, |ext| ext == "hxs")
    }

    async fn compact_tessellation(&self, tess: &str) -> Result<()> {
        let dir = PathBuf::from(format!("{}/{}", &self.config.storage.path, tess));
        if !dir.exists() {
            return Ok(());
        }

        let now = Utc::now().timestamp_millis();
        let mut all_docs: HashMap<Ulid, Document> = HashMap::new();
        let mut to_delete = Vec::new();

        let mut files = fs::read_dir(&dir).await?;
        while let Some(entry) = files.next_entry().await? {
            let path = entry.path();
            if SstUtil::is_sstable(&path) {
                let map = SstReader::read(&path)?;
                let mut retained = 0;
                for (id, doc) in map {
                    if doc.ttl.map_or(true, |ttl| ttl >= now) {
                        all_docs.insert(id, doc);
                        retained += 1;
                    }
                }

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
        SstWriter::write(
            &self.config,
            &compact_path,
            all_docs.clone().into_iter().collect(),
        )?;

        for path in &to_delete {
            debug!("🗑️  Deleted obsolete SSTable: {:?}", path);
            let _ = fs::remove_file(path).await;
        }

        info!(
            "🔧 Compacted {} SSTables for '{}', retained {} documents.",
            to_delete.len(),
            tess,
            all_docs.len()
        );

        Ok(())
    }

    /// Count all non-expired documents currently stored on disk.
    pub fn count_documents_on_disk(&self) -> Result<usize> {
        let base = Path::new(&self.config.storage.path);
        if !base.exists() {
            return Ok(0);
        }

        let now = Utc::now().timestamp_millis();
        let mut total = 0;

        for tess_entry in std::fs::read_dir(base)? {
            let tess_path = tess_entry?.path();
            if !tess_path.is_dir() {
                continue;
            }

            for file in std::fs::read_dir(tess_path)? {
                let path = file?.path();
                if SstUtil::is_sstable(&path) {
                    let map = SstReader::read(&path)?;
                    total += map
                        .values()
                        .filter(|doc| doc.ttl.map_or(true, |ttl| ttl >= now))
                        .count();
                }
            }
        }

        Ok(total)
    }

    /// Tracks total compressed document size (on disk, SST format).
    pub fn track_document_size(&self, doc: &Document) {
        if let Ok(raw) = serde_json::to_vec(doc) {
            if let Ok(comp) =
                encode_all(Cursor::new(raw), self.config.compression.compression_level)
            {
                let mut entry_size = 16 + 1 + 4 + comp.len(); // id (16) + flags (1) + len (4) + compressed data
                if doc.ttl.is_some() {
                    entry_size += 8; // TTL
                }

                self.total_doc_bytes
                    .fetch_add(entry_size, Ordering::Relaxed);
            }
        }
    }
}
