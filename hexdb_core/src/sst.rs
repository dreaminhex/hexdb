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
// Document bodies are encrypted with AES-256-GCM using the storage key ring
// (version 3). The authenticated data binds each body to its document ID and
// sequence number, so bodies can't be swapped between entries. The index
// (IDs, sequence numbers, TTLs, offsets) is not encrypted. Version 2 files
// (unencrypted) are still read, and compaction rewrites them encrypted, as it
// does files written with a previous key.
//
// File layout (version 3, all integers big-endian):
//   Header (64 bytes)
//     0x00 MAGIC "HXDB"           4
//     0x04 VERSION (3)            2
//     0x06 COMPRESSION (1 = zstd) 1
//     0x07 ENCRYPTION             1   (0 = none, 1 = AES-256-GCM)
//     0x08 entry count            8
//     0x10 created (epoch ms)     8
//     0x18 index offset           8
//     0x20 index size             8
//     0x28 index checksum         8   (first 8 bytes of BLAKE3 over the index block)
//     0x30 max sequence number    8
//     0x38 key ID                 8   (KeyRing::key_id of the encryption key)
//   Entries, from 0x40, each:
//     id (16) | flags (1: bit0 TTL, bit1 tombstone) | seq (8) | [ttl (8)] | len (4) | body
//     body = nonce (12) | AES-256-GCM(zstd(JSON document)), AAD = id | seq
//   Index block, sorted by id, each:
//     id (16) | flags (1) | seq (8) | [ttl (8)] | entry offset (8) | len (4)

use crate::{crypt::KeyRing, document::Document, wal::sync_dir};
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
const VERSION: u16 = 3;
const LEGACY_VERSION: u16 = 2;
const ENCRYPTION_AES_GCM: u8 = 1;
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
    /// ID of the key the bodies are encrypted with; `None` for unencrypted (version 2) files.
    pub key_id: Option<u64>,
    keys: Arc<KeyRing>,
}

/// Authenticated data for an entry body.
fn body_aad(id: &Ulid, seq: u64) -> [u8; 24] {
    let mut aad = [0u8; 24];
    aad[..16].copy_from_slice(&id.to_bytes());
    aad[16..].copy_from_slice(&seq.to_be_bytes());
    aad
}

impl SstFile {
    /// Write entries to a new SSTable at `path` (atomically) and open it.
    /// `seq_floor` raises the recorded max sequence number (used by compaction so
    /// the highest sequence number on disk never goes down when entries are dropped).
    pub fn write(path: &Path, entries: Vec<SstEntry>, compression_level: i32, seq_floor: u64, keys: &Arc<KeyRing>) -> Result<SstFile> {
        Self::write_impl(path, entries, compression_level, seq_floor, keys, true)
    }

    /// `encrypt = false` writes an unencrypted version 2 file (tests of the upgrade path only).
    fn write_impl(
        path: &Path,
        mut entries: Vec<SstEntry>,
        compression_level: i32,
        seq_floor: u64,
        keys: &Arc<KeyRing>,
        encrypt: bool,
    ) -> Result<SstFile> {
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
                Some(data) if encrypt => keys.encrypt(&encode_all(&data[..], compression_level)?, &body_aad(&entry.id, entry.seq))?,
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
        header.write_u16::<BigEndian>(if encrypt { VERSION } else { LEGACY_VERSION })?;
        header.write_u8(COMPRESSION_ZSTD)?;
        header.write_u8(if encrypt { ENCRYPTION_AES_GCM } else { 0 })?;
        header.write_u64::<BigEndian>(entries.len() as u64)?;
        header.write_i64::<BigEndian>(created)?;
        header.write_u64::<BigEndian>(offset)?;
        header.write_u64::<BigEndian>(index_block.len() as u64)?;
        header.write_u64::<BigEndian>(checksum)?;
        header.write_u64::<BigEndian>(max_seq)?;
        header.write_u64::<BigEndian>(if encrypt { keys.current_id() } else { 0 })?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;

        let file = file.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path).with_context(|| format!("Failed to move {} into place", path.display()))?;
        if let Some(dir) = path.parent() {
            sync_dir(dir);
        }

        SstFile::open(path, keys)
    }

    /// Open an SSTable and load its index.
    pub fn open(path: &Path, keys: &Arc<KeyRing>) -> Result<SstFile> {
        let mut file = BufReader::new(File::open(path).with_context(|| format!("Failed to open {}", path.display()))?);
        let size_bytes = file.get_ref().metadata()?.len();

        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if &magic != MAGIC {
            bail!("{} is not an SSTable (bad magic header)", path.display());
        }
        let version = file.read_u16::<BigEndian>()?;
        if version != VERSION && version != LEGACY_VERSION {
            bail!(
                "{} is SSTable version {}, but this HexDB reads versions {} and {}. Files from older HexDB builds can't be read; move them out of the data directory.",
                path.display(),
                version,
                LEGACY_VERSION,
                VERSION
            );
        }
        let compression = file.read_u8()?;
        if compression != COMPRESSION_ZSTD {
            bail!("{} uses unsupported compression {}", path.display(), compression);
        }
        let encryption = file.read_u8()?;
        let entry_count = file.read_u64::<BigEndian>()?;
        let created = file.read_i64::<BigEndian>()?;
        let index_offset = file.read_u64::<BigEndian>()?;
        let index_size = file.read_u64::<BigEndian>()?;
        let expected_checksum = file.read_u64::<BigEndian>()?;
        let max_seq = file.read_u64::<BigEndian>()?;
        let header_key_id = file.read_u64::<BigEndian>()?;
        let key_id = match (version, encryption) {
            (LEGACY_VERSION, _) | (_, 0) => None,
            (_, ENCRYPTION_AES_GCM) => {
                if !keys.has_key(header_key_id) {
                    bail!(
                        "{} is encrypted with key {:016x}, which isn't configured. Add that key to storage.previous_encryption_keys (or restore storage.encryption_key).",
                        path.display(),
                        header_key_id
                    );
                }
                Some(header_key_id)
            }
            (_, other) => bail!("{} uses unsupported encryption {}", path.display(), other),
        };

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

        Ok(SstFile { path: path.to_path_buf(), max_seq, created, size_bytes, index, key_id, keys: keys.clone() })
    }

    /// Read and decompress one entry's document JSON.
    pub fn read_entry(&self, entry: &IndexEntry) -> Result<Vec<u8>> {
        let mut file = File::open(&self.path).with_context(|| format!("Failed to open {}", self.path.display()))?;
        file.seek(SeekFrom::Start(entry.offset))?;
        let mut header = [0u8; 16 + 1 + 8];
        file.read_exact(&mut header)?;
        let id = Ulid::from_bytes(header[..16].try_into().unwrap());
        if entry.ttl.is_some() {
            file.seek(SeekFrom::Current(8))?;
        }
        let len = file.read_u32::<BigEndian>()?;
        if len != entry.len {
            bail!("{} is corrupt (entry length mismatch)", self.path.display());
        }
        let mut body = vec![0u8; len as usize];
        file.read_exact(&mut body)?;
        let compressed = match self.key_id {
            Some(key_id) => self
                .keys
                .decrypt(key_id, &body, &body_aad(&id, entry.seq))
                .with_context(|| format!("{}: document {} can't be decrypted", self.path.display(), id))?,
            None => body,
        };
        Ok(decode_all(&compressed[..])?)
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
    keys: Arc<KeyRing>,
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
    pub fn open(base: &Path, compression_level: i32, keys: Arc<KeyRing>) -> Result<SstStore> {
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
                            tables.entry(tess.clone()).or_default().push(Arc::new(SstFile::open(&path, &keys)?));
                        }
                        Some("tmp") => {
                            warn!("🧹 Removing incomplete SSTable {}.", path.display());
                            let _ = fs::remove_file(&path);
                        }
                        _ => {}
                    }
                }
            }
        }

        let files: usize = tables.values().map(Vec::len).sum();
        info!("📚 Opened {} SSTables across {} tessellations.", files, tables.len());
        Ok(SstStore {
            base: base.to_path_buf(),
            compression_level,
            keys,
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
        let keys = self.keys.clone();
        let file = tokio::task::spawn_blocking(move || -> Result<SstFile> {
            fs::create_dir_all(&dir)?;
            SstFile::write(&path, entries, level, 0, &keys)
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
            warn!("🧹 Removing SSTables left over from dropped tessellation '{}'.", tess);
            self.drop_tessellation(tess).await?;
        }
        Ok(())
    }

    /// SSTables not encrypted with the current key (unencrypted, or a
    /// previous key). Compaction rewrites them.
    pub async fn files_needing_rewrite(&self) -> usize {
        let current = Some(self.keys.current_id());
        self.tables.read().await.values().flatten().filter(|f| f.key_id != current).count()
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
            // Rewrite files that aren't under the current key (key rotation, or
            // files from before encryption).
            let stale_key = files.iter().any(|f| f.key_id != Some(self.keys.current_id()));
            if files.len() < 2 && drop_count == 0 && !stale_key {
                continue;
            }

            let level = self.compression_level;
            let keys = self.keys.clone();
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
                Ok(Some(SstFile::write(&path, entries, level, seq_floor, &keys)?))
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
                    warn!("⚠️ Failed to delete compacted SSTable {}: {}", old.path.display(), e);
                }
            }
            sync_dir(&dir);

            stats.tessellations += 1;
            stats.files_merged += files.len();
            stats.entries_kept += kept_count;
            stats.entries_dropped += total_entries - kept_count;
            debug!("🗜ï¸  Compacted {} SSTables for '{}' ({} entries kept).", files.len(), tess, kept_count);
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

    fn test_keys() -> Arc<KeyRing> {
        Arc::new(KeyRing::new(&[7u8; 32], &[]))
    }

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
            &test_keys(),
        )
        .unwrap();

        assert_eq!(file.max_seq, 6);
        let ea = file.index[&a];
        assert_eq!((ea.seq, ea.ttl, ea.tombstone), (5, Some(99), false));
        assert_eq!(file.read_entry(&ea).unwrap(), b"{\"a\":1}");
        assert!(file.index[&b].tombstone);

        let reopened = SstFile::open(&path, &test_keys()).unwrap();
        assert_eq!(reopened.index.len(), 2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bodies_are_encrypted_and_bound_to_their_entry() {
        let dir = temp_dir();
        let path = dir.join("x.hxs");
        let secret = b"{\"card\":\"4111-1111-1111-1111\"}".to_vec();
        let (a, b) = (Ulid::new(), Ulid::new());
        let file = SstFile::write(
            &path,
            vec![
                SstEntry { id: a, seq: 1, ttl: None, data: Some(secret.clone()) },
                SstEntry { id: b, seq: 2, ttl: None, data: Some(b"{\"x\":1}".to_vec()) },
            ],
            0,
            0,
            &test_keys(),
        )
        .unwrap();
        let raw = fs::read(&path).unwrap();
        assert!(!raw.windows(9).any(|w| w == b"4111-1111"), "plaintext must not reach the disk");
        assert_eq!(file.key_id, Some(test_keys().current_id()));

        // Swap the two bodies on disk: authentication fails instead of returning the wrong document.
        let (ea, eb) = (file.index[&a], file.index[&b]);
        let body = |e: &IndexEntry| (e.offset as usize + 16 + 1 + 8 + 4, e.len as usize);
        let ((oa, la), (ob, lb)) = (body(&ea), body(&eb));
        let mut tampered = raw.clone();
        let (ba, bb) = (raw[oa..oa + la].to_vec(), raw[ob..ob + lb].to_vec());
        if la == lb {
            tampered[oa..oa + la].copy_from_slice(&bb);
            tampered[ob..ob + lb].copy_from_slice(&ba);
        } else {
            tampered[oa + 20] ^= 0x01;
        }
        fs::write(&path, &tampered).unwrap();
        let reopened = SstFile::open(&path, &test_keys()).unwrap();
        assert!(reopened.read_entry(&reopened.index[&a]).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn rotation_and_legacy_files_are_rewritten_by_compaction() {
        let dir = temp_dir();
        let tess_dir = dir.join("t");
        fs::create_dir_all(&tess_dir).unwrap();
        let (old_key, new_key) = ([1u8; 32], [2u8; 32]);
        let old = Arc::new(KeyRing::new(&old_key, &[]));
        let (a, b) = (Ulid::new(), Ulid::new());
        SstFile::write(&tess_dir.join("a.hxs"), vec![SstEntry { id: a, seq: 1, ttl: None, data: Some(b"{\"v\":\"a\"}".to_vec()) }], 0, 0, &old).unwrap();
        SstFile::write_impl(&tess_dir.join("b.hxs"), vec![SstEntry { id: b, seq: 2, ttl: None, data: Some(b"{\"v\":\"b\"}".to_vec()) }], 0, 0, &old, false).unwrap();

        // Without the old key, its files are refused with a clear message.
        let err = SstStore::open(&dir, 0, Arc::new(KeyRing::new(&new_key, &[]))).err().unwrap().to_string();
        assert!(err.contains("previous_encryption_keys"), "{}", err);

        // With it as a previous key, everything reads, and compaction moves it all to the new key.
        let store = SstStore::open(&dir, 0, Arc::new(KeyRing::new(&new_key, &[old_key]))).unwrap();
        assert_eq!(store.files_needing_rewrite().await, 2);
        for id in [a, b] {
            let Some(DiskLookup::Live { file, entry }) = store.lookup("t", &id).await else { panic!() };
            assert!(file.read_entry(&entry).is_ok());
        }
        store.compact(0, 0).await.unwrap();
        assert_eq!(store.files_needing_rewrite().await, 0);
        drop(store);

        let store = SstStore::open(&dir, 0, Arc::new(KeyRing::new(&new_key, &[]))).unwrap();
        let Some(DiskLookup::Live { file, entry }) = store.lookup("t", &b).await else { panic!() };
        assert_eq!(file.read_entry(&entry).unwrap(), b"{\"v\":\"b\"}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detects_corrupt_index() {
        let dir = temp_dir();
        let path = dir.join("x.hxs");
        SstFile::write(&path, vec![SstEntry { id: Ulid::new(), seq: 1, ttl: None, data: Some(b"{}".to_vec()) }], 0, 0, &test_keys())
            .unwrap();
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        fs::write(&path, bytes).unwrap();
        assert!(SstFile::open(&path, &test_keys()).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn newest_version_wins_and_compaction_drops_tombstones() {
        let dir = temp_dir();
        let store = SstStore::open(&dir, 0, test_keys()).unwrap();
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
        let reopened = SstStore::open(&dir, 0, test_keys()).unwrap();
        assert_eq!(reopened.max_seq().await, 3, "max seq survives dropping the tombstone");
        fs::remove_dir_all(&dir).ok();
    }
}
