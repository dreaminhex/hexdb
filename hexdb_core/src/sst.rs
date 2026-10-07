// HexDB Core SSTable Module
//
// SSTables are immutable files holding flushed document versions for one
// tessellation (`<storage>/<tessellation>/<ulid>.hxs`). Each entry carries the
// sequence number of the write that produced it, an optional TTL, and a
// tombstone flag for deletes. When the same document appears in several
// files, the entry with the highest sequence number wins.
//
// Only each file's index is kept in memory; document bodies are read from
// disk on demand. Files are written to a temporary name, fsynced, and renamed
// into place, so a crash never leaves a half-written table.
//
// File layout (version 2, all integers big-endian):
//   Header (64 bytes)
//     0x00 MAGIC "HXDB"           4
//     0x04 VERSION (2)            2
//     0x06 COMPRESSION (1 = zstd) 1
//     0x07 reserved               1
//     0x08 entry count            8
//     0x10 created (epoch ms)     8
//     0x18 index offset           8
//     0x20 index size             8
//     0x28 index checksum         8   (first 8 bytes of BLAKE3 over the index block)
//     0x30 max sequence number    8
//     0x38 reserved               8
//   Entries, from 0x40, each:
//     id (16) | flags (1: bit0 TTL, bit1 tombstone) | seq (8) | [ttl (8)] | len (4) | zstd(JSON document)
//   Index block, sorted by id, each:
//     id (16) | flags (1) | seq (8) | [ttl (8)] | entry offset (8) | len (4)

use crate::{document::Document, wal::sync_dir};
use anyhow::{anyhow, bail, Context, Result};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};
use ulid::Ulid;
use zstd::stream::{decode_all, encode_all};

const MAGIC: &[u8; 4] = b"HXDB";
const VERSION: u16 = 2;
const COMPRESSION_ZSTD: u8 = 1;
const HEADER_LEN: u64 = 64;
const FLAG_TTL: u8 = 0b01;
const FLAG_TOMBSTONE: u8 = 0b10;
pub const SST_EXTENSION: &str = "hxs";

/// One entry to write into an SSTable.
#[derive(Debug, Clone)]
pub struct SstEntry {
    pub id: Ulid,
    pub seq: u64,
    pub ttl: Option<i64>,
    /// Uncompressed JSON document, or `None` for a tombstone.
    pub data: Option<Vec<u8>>,
}

/// Index information for one entry in an SSTable.
#[derive(Debug, Clone, Copy)]
pub struct IndexEntry {
    pub seq: u64,
    pub ttl: Option<i64>,
    pub tombstone: bool,
    pub offset: u64,
    /// Compressed body length.
    pub len: u32,
}

impl IndexEntry {
    pub fn is_expired(&self, now_millis: i64) -> bool {
        self.ttl.is_some_and(|ttl| ttl <= now_millis)
    }
}

/// An open SSTable: its path and in-memory index.
#[derive(Debug)]
pub struct SstFile {
    pub path: PathBuf,
    pub max_seq: u64,
    pub created: i64,
    pub size_bytes: u64,
    pub index: HashMap<Ulid, IndexEntry>,
}

impl SstFile {
    /// Write entries to a new SSTable at `path` (atomically) and open it.
    /// `seq_floor` raises the recorded max sequence number (used by compaction so
    /// the highest sequence number on disk never goes down when entries are dropped).
    pub fn write(path: &Path, mut entries: Vec<SstEntry>, compression_level: i32, seq_floor: u64) -> Result<SstFile> {
        entries.sort_by_key(|e| e.id);
        let created = chrono::Utc::now().timestamp_millis();
        let max_seq = entries.iter().map(|e| e.seq).max().unwrap_or(0).max(seq_floor);

        let tmp = path.with_extension("tmp");
        let mut file = BufWriter::new(File::create(&tmp).with_context(|| format!("Failed to create {}", tmp.display()))?);
        file.write_all(&[0u8; HEADER_LEN as usize])?;

        let mut index = Vec::with_capacity(entries.len());
        let mut offset = HEADER_LEN;
        for entry in &entries {
            let mut flags = 0u8;
            if entry.ttl.is_some() {
                flags |= FLAG_TTL;
            }
            let body = match &entry.data {
                Some(data) => encode_all(&data[..], compression_level)?,
                None => {
                    flags |= FLAG_TOMBSTONE;
                    Vec::new()
                }
            };

            let entry_offset = offset;
            let mut header = Vec::with_capacity(37);
            header.write_all(&entry.id.to_bytes())?;
            header.write_u8(flags)?;
            header.write_u64::<BigEndian>(entry.seq)?;
            if let Some(ttl) = entry.ttl {
                header.write_i64::<BigEndian>(ttl)?;
            }
            header.write_u32::<BigEndian>(body.len() as u32)?;
            file.write_all(&header)?;
            file.write_all(&body)?;
            offset += (header.len() + body.len()) as u64;

            index.push((entry.id, flags, entry.seq, entry.ttl, entry_offset, body.len() as u32));
        }

        let mut index_block = Vec::new();
        for (id, flags, seq, ttl, entry_offset, len) in &index {
            index_block.write_all(&id.to_bytes())?;
            index_block.write_u8(*flags)?;
            index_block.write_u64::<BigEndian>(*seq)?;
            if let Some(ttl) = ttl {
                index_block.write_i64::<BigEndian>(*ttl)?;
            }
            index_block.write_u64::<BigEndian>(*entry_offset)?;
            index_block.write_u32::<BigEndian>(*len)?;
        }
        file.write_all(&index_block)?;
        let checksum = checksum(&index_block);

        let mut header = Vec::with_capacity(HEADER_LEN as usize);
        header.write_all(MAGIC)?;
        header.write_u16::<BigEndian>(VERSION)?;
        header.write_u8(COMPRESSION_ZSTD)?;
        header.write_u8(0)?;
        header.write_u64::<BigEndian>(entries.len() as u64)?;
        header.write_i64::<BigEndian>(created)?;
        header.write_u64::<BigEndian>(offset)?;
        header.write_u64::<BigEndian>(index_block.len() as u64)?;
        header.write_u64::<BigEndian>(checksum)?;
        header.write_u64::<BigEndian>(max_seq)?;
        header.write_u64::<BigEndian>(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;

        let file = file.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path).with_context(|| format!("Failed to move {} into place", path.display()))?;
        if let Some(dir) = path.parent() {
            sync_dir(dir);
        }

        SstFile::open(path)
    }

    /// Open an SSTable and load its index.
    pub fn open(path: &Path) -> Result<SstFile> {
        let mut file = BufReader::new(File::open(path).with_context(|| format!("Failed to open {}", path.display()))?);
        let size_bytes = file.get_ref().metadata()?.len();

        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if &magic != MAGIC {
            bail!("{} is not an SSTable (bad magic header)", path.display());
        }
        let version = file.read_u16::<BigEndian>()?;
        if version != VERSION {
            bail!(
                "{} is SSTable version {}, but this HexDB reads version {}. Files from older HexDB builds can't be read; move them out of the data directory.",
                path.display(),
                version,
                VERSION
            );
        }
        let compression = file.read_u8()?;
        if compression != COMPRESSION_ZSTD {
            bail!("{} uses unsupported compression {}", path.display(), compression);
        }
        let _reserved = file.read_u8()?;
        let entry_count = file.read_u64::<BigEndian>()?;
        let created = file.read_i64::<BigEndian>()?;
        let index_offset = file.read_u64::<BigEndian>()?;
        let index_size = file.read_u64::<BigEndian>()?;
        let expected_checksum = file.read_u64::<BigEndian>()?;
        let max_seq = file.read_u64::<BigEndian>()?;

        if index_offset.checked_add(index_size).is_none_or(|end| end > size_bytes) {
            bail!("{} is truncated or corrupt (index outside the file)", path.display());
        }
        file.seek(SeekFrom::Start(index_offset))?;
        let mut index_block = vec![0u8; index_size as usize];
        file.read_exact(&mut index_block)?;
        if checksum(&index_block) != expected_checksum {
            bail!("{} is corrupt (index checksum mismatch)", path.display());
        }

        let mut index = HashMap::with_capacity(entry_count as usize);
        let mut cursor = Cursor::new(&index_block[..]);
        for _ in 0..entry_count {
            let mut id = [0u8; 16];
            cursor.read_exact(&mut id)?;
            let flags = cursor.read_u8()?;
            let seq = cursor.read_u64::<BigEndian>()?;
            let ttl = if flags & FLAG_TTL != 0 { Some(cursor.read_i64::<BigEndian>()?) } else { None };
            let offset = cursor.read_u64::<BigEndian>()?;
            let len = cursor.read_u32::<BigEndian>()?;
            index.insert(
                Ulid::from_bytes(id),
                IndexEntry { seq, ttl, tombstone: flags & FLAG_TOMBSTONE != 0, offset, len },
            );
        }

        Ok(SstFile { path: path.to_path_buf(), max_seq, created, size_bytes, index })
    }

    /// Read and decompress one entry's document JSON.
    pub fn read_entry(&self, entry: &IndexEntry) -> Result<Vec<u8>> {
        let mut file = File::open(&self.path).with_context(|| format!("Failed to open {}", self.path.display()))?;
        file.seek(SeekFrom::Start(entry.offset))?;
        let mut header = [0u8; 16 + 1 + 8];
        file.read_exact(&mut header)?;
        if entry.ttl.is_some() {
            file.seek(SeekFrom::Current(8))?;
        }
        let len = file.read_u32::<BigEndian>()?;
        if len != entry.len {
            bail!("{} is corrupt (entry length mismatch)", self.path.display());
        }
        let mut body = vec![0u8; len as usize];
        file.read_exact(&mut body)?;
        Ok(decode_all(&body[..])?)
    }
}

fn checksum(bytes: &[u8]) -> u64 {
    let hash = blake3::hash(bytes);
    u64::from_be_bytes(hash.as_bytes()[..8].try_into().unwrap())
}

/// The newest on-disk version of a document.
pub enum DiskLookup {
    Live { file: Arc<SstFile>, entry: IndexEntry },
    Tombstone { seq: u64 },
}

/// All SSTables, grouped by tessellation.
pub struct SstStore {
    base: PathBuf,
    compression_level: i32,
    tables: RwLock<HashMap<String, Vec<Arc<SstFile>>>>,
    compaction: Mutex<()>,
}

/// Summary of a compaction run.
#[derive(Debug, Default, Clone, Copy)]
pub struct CompactionStats {
    pub tessellations: usize,
    pub files_merged: usize,
    pub entries_kept: usize,
    pub entries_dropped: usize,
}

impl SstStore {
    /// Open every SSTable under `base`. Leftover temporary files from an
    /// interrupted write are removed.
    pub fn open(base: &Path, compression_level: i32) -> Result<SstStore> {
        let mut tables: HashMap<String, Vec<Arc<SstFile>>> = HashMap::new();
        if base.exists() {
            for entry in fs::read_dir(base)? {
                let dir = entry?.path();
                if !dir.is_dir() {
                    continue;
                }
                let Some(tess) = dir.file_name().and_then(|n| n.to_str()).map(String::from) else { continue };
                if tess == "wal" {
                    continue;
                }
                for file in fs::read_dir(&dir)? {
                    let path = file?.path();
                    match path.extension().and_then(|e| e.to_str()) {
                        Some(SST_EXTENSION) => {
                            tables.entry(tess.clone()).or_default().push(Arc::new(SstFile::open(&path)?));
                        }
                        Some("tmp") => {
                            warn!("ðŸ§¹ Removing incomplete SSTable {}.", path.display());
                            let _ = fs::remove_file(&path);
                        }
                        _ => {}
                    }
                }
            }
        }

        let files: usize = tables.values().map(Vec::len).sum();
        info!("ðŸ“š Opened {} SSTables across {} tessellations.", files, tables.len());
        Ok(SstStore {
            base: base.to_path_buf(),
            compression_level,
            tables: RwLock::new(tables),
            compaction: Mutex::new(()),
        })
    }

    /// Highest sequence number in any SSTable.
    pub async fn max_seq(&self) -> u64 {
        self.tables
            .read()
            .await
            .values()
            .flatten()
            .map(|f| f.max_seq)
            .max()
            .unwrap_or(0)
    }

    pub async fn tessellations(&self) -> Vec<String> {
        self.tables.read().await.keys().cloned().collect()
    }

    /// Sequence number of the newest on-disk version of a document, if any.
    pub async fn seq_of(&self, tess: &str, id: &Ulid) -> Option<u64> {
        let tables = self.tables.read().await;
        tables
            .get(tess)?
            .iter()
            .filter_map(|f| f.index.get(id).map(|e| e.seq))
            .max()
    }

    /// Find the newest on-disk version of a document.
    pub async fn lookup(&self, tess: &str, id: &Ulid) -> Option<DiskLookup> {
        let tables = self.tables.read().await;
        let (file, entry) = tables
            .get(tess)?
            .iter()
            .filter_map(|f| f.index.get(id).map(|e| (f, *e)))
            .max_by_key(|(_, e)| e.seq)?;
        Some(if entry.tombstone {
            DiskLookup::Tombstone { seq: entry.seq }
        } else {
            DiskLookup::Live { file: file.clone(), entry }
        })
    }

    /// Newest on-disk index entry for every document in a tessellation.
    pub async fn latest_entries(&self, tess: &str) -> HashMap<Ulid, (Arc<SstFile>, IndexEntry)> {
        let tables = self.tables.read().await;
        let mut latest: HashMap<Ulid, (Arc<SstFile>, IndexEntry)> = HashMap::new();
        for file in tables.get(tess).into_iter().flatten() {
            for (id, entry) in &file.index {
                match latest.get(id) {
                    Some((_, existing)) if existing.seq >= entry.seq => {}
                    _ => {
                        latest.insert(*id, (file.clone(), *entry));
                    }
                }
            }
        }
        latest
    }

    /// Write a new SSTable for a tessellation and make it visible to readers.
    pub async fn add_table(&self, tess: &str, entries: Vec<SstEntry>) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let dir = self.base.join(tess);
        let path = dir.join(format!("{}.{}", Ulid::new(), SST_EXTENSION));
        let level = self.compression_level;
        let file = tokio::task::spawn_blocking(move || -> Result<SstFile> {
            fs::create_dir_all(&dir)?;
            SstFile::write(&path, entries, level, 0)
        })
        .await??;

        self.tables
            .write()
            .await
            .entry(tess.to_string())
            .or_default()
            .push(Arc::new(file));
        Ok(())
    }

    /// Remove a tessellation's SSTables from disk.
    pub async fn drop_tessellation(&self, tess: &str) -> Result<()> {
        let _guard = self.compaction.lock().await;
        self.tables.write().await.remove(tess);
        let dir = self.base.join(tess);
        if dir.exists() {
            fs::remove_dir_all(&dir).with_context(|| format!("Failed to delete {}", dir.display()))?;
            sync_dir(&self.base);
        }
        Ok(())
    }

    /// Ignore (and delete) on-disk entries for documents in a dropped tessellation
    /// that were written at or before `dropped_seq`.
    pub async fn purge_dropped(&self, tess: &str, dropped_seq: u64) -> Result<()> {
        let has_old = self
            .tables
            .read()
            .await
            .get(tess)
            .is_some_and(|files| files.iter().any(|f| f.index.values().any(|e| e.seq <= dropped_seq)));
        if has_old {
            warn!("ðŸ§¹ Removing SSTables left over from dropped tessellation '{}'.", tess);
            self.drop_tessellation(tess).await?;
        }
        Ok(())
    }

    /// Total bytes and file count on disk.
    pub async fn disk_usage(&self) -> (u64, usize) {
        let tables = self.tables.read().await;
        let files: Vec<&Arc<SstFile>> = tables.values().flatten().collect();
        (files.iter().map(|f| f.size_bytes).sum(), files.len())
    }

    /// Merge every SSTable of each tessellation into one file. The newest version
    /// of each document is kept. Tombstones and expired documents are dropped
    /// when their sequence number is below `drop_floor`, meaning no older
    /// version can still be waiting in the WAL. Tessellations with one file are
    /// only rewritten when they have something to drop.
    pub async fn compact(&self, drop_floor: u64, now_millis: i64) -> Result<CompactionStats> {
        let _guard = self.compaction.lock().await;
        let mut stats = CompactionStats::default();

        let snapshot: Vec<(String, Vec<Arc<SstFile>>)> = self
            .tables
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        for (tess, files) in snapshot {
            if files.is_empty() {
                continue;
            }

            let mut latest: HashMap<Ulid, (Arc<SstFile>, IndexEntry)> = HashMap::new();
            let mut total_entries = 0;
            for file in &files {
                for (id, entry) in &file.index {
                    total_entries += 1;
                    match latest.get(id) {
                        Some((_, existing)) if existing.seq >= entry.seq => {}
                        _ => {
                            latest.insert(*id, (file.clone(), *entry));
                        }
                    }
                }
            }

            let droppable = |e: &IndexEntry| (e.tombstone || e.is_expired(now_millis)) && e.seq < drop_floor;
            let drop_count = latest.values().filter(|(_, e)| droppable(e)).count();
            if files.len() < 2 && drop_count == 0 {
                continue;
            }

            let level = self.compression_level;
            let kept: Vec<(Arc<SstFile>, Ulid, IndexEntry)> = latest
                .into_iter()
                .filter(|(_, (_, e))| !droppable(e))
                .map(|(id, (f, e))| (f, id, e))
                .collect();
            let kept_count = kept.len();
            let dir = self.base.join(&tess);
            let write_dir = dir.clone();
            // Keep the highest sequence number even if every entry is dropped, so
            // sequence numbers are never reused after a restart.
            let seq_floor = files.iter().map(|f| f.max_seq).max().unwrap_or(0);
            let new_file = tokio::task::spawn_blocking(move || -> Result<Option<SstFile>> {
                let mut entries = Vec::with_capacity(kept.len());
                for (file, id, entry) in kept {
                    let data = if entry.tombstone { None } else { Some(file.read_entry(&entry)?) };
                    entries.push(SstEntry { id, seq: entry.seq, ttl: entry.ttl, data });
                }
                let path = write_dir.join(format!("{}.{}", Ulid::new(), SST_EXTENSION));
                Ok(Some(SstFile::write(&path, entries, level, seq_floor)?))
            })
            .await??;

            // Swap the merged file in for exactly the files it replaces; files
            // flushed meanwhile stay.
            {
                let mut tables = self.tables.write().await;
                let list = tables.entry(tess.clone()).or_default();
                list.retain(|f| !files.iter().any(|old| Arc::ptr_eq(old, f)));
                if let Some(file) = new_file {
                    list.push(Arc::new(file));
                }
            }
            for old in &files {
                if let Err(e) = fs::remove_file(&old.path) {
                    warn!("âš ï¸ Failed to delete compacted SSTable {}: {}", old.path.display(), e);
                }
            }
            sync_dir(&dir);

            stats.tessellations += 1;
            stats.files_merged += files.len();
            stats.entries_kept += kept_count;
            stats.entries_dropped += total_entries - kept_count;
            debug!("ðŸ—œï¸  Compacted {} SSTables for '{}' ({} entries kept).", files.len(), tess, kept_count);
        }

        Ok(stats)
    }
}

/// Parse a document from SSTable or memtable bytes.
pub fn parse_document(bytes: &[u8]) -> Result<Document> {
    serde_json::from_slice(bytes).map_err(|e| anyhow!("Stored document is unreadable: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hexdb-sst-test-{}", Ulid::new()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_and_reads_entries() {
        let dir = temp_dir();
        let a = Ulid::new();
        let b = Ulid::new();
        let path = dir.join("x.hxs");
        let file = SstFile::write(
            &path,
            vec![
                SstEntry { id: a, seq: 5, ttl: Some(99), data: Some(b"{\"a\":1}".to_vec()) },
                SstEntry { id: b, seq: 6, ttl: None, data: None },
            ],
            3,
            0,
        )
        .unwrap();

        assert_eq!(file.max_seq, 6);
        let ea = file.index[&a];
        assert_eq!((ea.seq, ea.ttl, ea.tombstone), (5, Some(99), false));
        assert_eq!(file.read_entry(&ea).unwrap(), b"{\"a\":1}");
        assert!(file.index[&b].tombstone);

        let reopened = SstFile::open(&path).unwrap();
        assert_eq!(reopened.index.len(), 2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detects_corrupt_index() {
        let dir = temp_dir();
        let path = dir.join("x.hxs");
        SstFile::write(&path, vec![SstEntry { id: Ulid::new(), seq: 1, ttl: None, data: Some(b"{}".to_vec()) }], 0, 0)
            .unwrap();
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        fs::write(&path, bytes).unwrap();
        assert!(SstFile::open(&path).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn newest_version_wins_and_compaction_drops_tombstones() {
        let dir = temp_dir();
        let store = SstStore::open(&dir, 0).unwrap();
        let id = Ulid::new();
        let other = Ulid::new();

        store
            .add_table("t", vec![
                SstEntry { id, seq: 1, ttl: None, data: Some(b"{\"v\":1}".to_vec()) },
                SstEntry { id: other, seq: 2, ttl: None, data: Some(b"{\"v\":9}".to_vec()) },
            ])
            .await
            .unwrap();
        store.add_table("t", vec![SstEntry { id, seq: 3, ttl: None, data: None }]).await.unwrap();

        assert!(matches!(store.lookup("t", &id).await, Some(DiskLookup::Tombstone { seq: 3 })));

        // Floor below the tombstone: it must be kept.
        let stats = store.compact(3, 0).await.unwrap();
        assert_eq!(stats.files_merged, 2);
        assert!(matches!(store.lookup("t", &id).await, Some(DiskLookup::Tombstone { seq: 3 })));

        // Floor above it: dropped.
        store.compact(10, 0).await.unwrap();
        assert!(store.lookup("t", &id).await.is_none());
        assert!(matches!(store.lookup("t", &other).await, Some(DiskLookup::Live { .. })));
        assert_eq!(store.disk_usage().await.1, 1);

        // Reopen from disk.
        let reopened = SstStore::open(&dir, 0).unwrap();
        assert_eq!(reopened.max_seq().await, 3, "max seq survives dropping the tombstone");
        fs::remove_dir_all(&dir).ok();
    }
}
