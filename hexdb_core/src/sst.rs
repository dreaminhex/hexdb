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
// Format (version 4, all integers big-endian). Everything but the header is
// encrypted with AES-256-GCM using the storage key ring:
//
//   Header (64 bytes)
//     0x00 MAGIC "HXDB"           4
//     0x04 VERSION (4)            2
//     0x06 COMPRESSION (1 = zstd) 1
//     0x07 ENCRYPTION (1 = AES-256-GCM) 1
//     0x08 entry count            8
//     0x10 created (epoch ms)     8
//     0x18 index offset           8
//     0x20 index size             8
//     0x28 index checksum         8   (first 8 bytes of BLAKE3 over the index block)
//     0x30 max sequence number    8
//     0x38 key ID                 8   (KeyRing::key_id of the encryption key)
//   Bodies, from 0x40: nonce (12) | AES-256-GCM(zstd(JSON document)), with
//     the document ID and sequence number as authenticated data, so a body
//     can't be moved to another entry. Tombstones have no body.
//   Index block: chunks of up to 64 index records, each chunk
//     length (4) | nonce (12) | AES-256-GCM(records), authenticated with its
//     chunk number. A record is
//     id (16) | flags (1: bit0 TTL, bit1 tombstone) | seq (8) | [ttl (8)] | body offset (8) | body length (4)
//
// Only a directory of chunks (first ID, offset, length) and a Bloom filter
// of the IDs stay in memory, about 2 bytes per document: a lookup checks the
// filter, finds the chunk by binary search, and reads and decrypts it (a few
// recently used chunks are cached). Both are rebuilt when the file is opened,
// which also verifies every chunk.
//
// Versions 2 (unencrypted) and 3 (encrypted bodies, plaintext index and
// entry headers) are still read, with their whole index in memory;
// compaction rewrites them as version 4, as it does files written with a
// previous key.

use crate::{crypt::KeyRing, document::Document, wal::sync_dir};
use anyhow::{anyhow, bail, Context, Result};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
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
const VERSION: u16 = 4;
/// Encrypted bodies, plaintext index (read, then rewritten by compaction).
const V3: u16 = 3;
const LEGACY_VERSION: u16 = 2;
/// Index records per encrypted index chunk.
const CHUNK_RECORDS: usize = 64;
/// Recently read index chunks kept per file.
const CHUNK_CACHE: usize = 16;
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

/// A table's index. Version 4 files keep only a chunk directory and a Bloom
/// filter in memory; older files keep every entry (sorted, compact).
#[derive(Debug)]
pub struct SstIndex {
    count: usize,
    kind: IndexKind,
}

#[derive(Debug)]
enum IndexKind {
    Loaded { ids: Vec<Ulid>, entries: Vec<IndexEntry> },
    Chunked(ChunkedIndex),
}

/// One decrypted chunk of an index: its records in ID order.
type ChunkRecords = Arc<Vec<(Ulid, IndexEntry)>>;

#[derive(Debug)]
struct ChunkedIndex {
    /// First ID, file offset, and length of each encrypted chunk.
    chunks: Vec<(Ulid, u64, u32)>,
    bloom: Bloom,
    handle: File,
    keys: Arc<KeyRing>,
    key_id: u64,
    path: PathBuf,
    cache: std::sync::Mutex<Vec<(usize, ChunkRecords)>>,
}

type Records = Arc<Vec<(Ulid, IndexEntry)>>;

impl ChunkedIndex {
    fn chunk(&self, n: usize) -> Records {
        if let Some((_, records)) = self.cache.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(i, _)| *i == n) {
            return records.clone();
        }
        let (_, offset, len) = self.chunks[n];
        let records = read_chunk(&self.handle, offset, len, &self.keys, self.key_id, n).unwrap_or_else(|e| {
            // Chunks were all verified at open; failing now means the file changed under us.
            tracing::error!("SSTable {} index chunk {} became unreadable: {:#}", self.path.display(), n, e);
            Vec::new()
        });
        let records = Arc::new(records);
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= CHUNK_CACHE {
            cache.remove(0);
        }
        cache.push((n, records.clone()));
        records
    }

    /// The chunk that would contain `id` (the last chunk starting at or before it).
    fn chunk_for(&self, id: &Ulid) -> Option<usize> {
        let n = self.chunks.partition_point(|(first, _, _)| first <= id);
        n.checked_sub(1)
    }
}

fn read_chunk(handle: &File, offset: u64, len: u32, keys: &KeyRing, key_id: u64, n: usize) -> Result<Vec<(Ulid, IndexEntry)>> {
    let mut sealed = vec![0u8; len as usize];
    read_exact_at(handle, &mut sealed, offset)?;
    let plain = keys.decrypt(key_id, &sealed, &chunk_aad(n))?;
    parse_records(&plain, None)
}

fn chunk_aad(n: usize) -> [u8; 16] {
    let mut aad = [0u8; 16];
    aad[..8].copy_from_slice(b"hxsindex");
    aad[8..].copy_from_slice(&(n as u64).to_be_bytes());
    aad
}

/// Parse index records (all of them, or exactly `count`).
fn parse_records(bytes: &[u8], count: Option<usize>) -> Result<Vec<(Ulid, IndexEntry)>> {
    let mut out = Vec::with_capacity(count.unwrap_or(CHUNK_RECORDS));
    let mut cursor = Cursor::new(bytes);
    while count.map_or((cursor.position() as usize) < bytes.len(), |c| out.len() < c) {
        let mut id = [0u8; 16];
        cursor.read_exact(&mut id)?;
        let flags = cursor.read_u8()?;
        let seq = cursor.read_u64::<BigEndian>()?;
        let ttl = if flags & FLAG_TTL != 0 { Some(cursor.read_i64::<BigEndian>()?) } else { None };
        let offset = cursor.read_u64::<BigEndian>()?;
        let len = cursor.read_u32::<BigEndian>()?;
        out.push((Ulid::from_bytes(id), IndexEntry { seq, ttl, tombstone: flags & FLAG_TOMBSTONE != 0, offset, len }));
    }
    Ok(out)
}

fn write_record(out: &mut Vec<u8>, id: &Ulid, entry: &IndexEntry) -> std::io::Result<()> {
    let mut flags = 0u8;
    if entry.ttl.is_some() {
        flags |= FLAG_TTL;
    }
    if entry.tombstone {
        flags |= FLAG_TOMBSTONE;
    }
    out.write_all(&id.to_bytes())?;
    out.write_u8(flags)?;
    out.write_u64::<BigEndian>(entry.seq)?;
    if let Some(ttl) = entry.ttl {
        out.write_i64::<BigEndian>(ttl)?;
    }
    out.write_u64::<BigEndian>(entry.offset)?;
    out.write_u32::<BigEndian>(entry.len)
}

/// A Bloom filter over document IDs (about 1% false positives).
#[derive(Debug)]
struct Bloom {
    bits: Vec<u64>,
    hashes: u32,
}

impl Bloom {
    fn new(count: usize) -> Self {
        let bits = (count.max(1) * 10).next_power_of_two();
        Bloom { bits: vec![0; bits.div_ceil(64)], hashes: 7 }
    }

    fn positions(&self, id: &Ulid) -> impl Iterator<Item = usize> + '_ {
        let v: u128 = (*id).into();
        let mix = |mut x: u64| {
            x ^= x >> 33;
            x = x.wrapping_mul(0xff51afd7ed558ccd);
            x ^= x >> 33;
            x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
            x ^ (x >> 33)
        };
        let (h1, h2) = (mix(v as u64), mix((v >> 64) as u64) | 1);
        let m = self.bits.len() * 64;
        (0..self.hashes as u64).map(move |i| (h1.wrapping_add(i.wrapping_mul(h2)) % m as u64) as usize)
    }

    fn insert(&mut self, id: &Ulid) {
        let positions: Vec<usize> = self.positions(id).collect();
        for p in positions {
            self.bits[p / 64] |= 1 << (p % 64);
        }
    }

    fn may_contain(&self, id: &Ulid) -> bool {
        self.positions(id).all(|p| self.bits[p / 64] & (1 << (p % 64)) != 0)
    }
}

impl SstIndex {
    fn loaded(mut pairs: Vec<(Ulid, IndexEntry)>) -> Self {
        pairs.sort_by_key(|(id, _)| *id);
        let count = pairs.len();
        let (ids, entries) = pairs.into_iter().unzip();
        SstIndex { count, kind: IndexKind::Loaded { ids, entries } }
    }

    pub fn get(&self, id: &Ulid) -> Option<IndexEntry> {
        match &self.kind {
            IndexKind::Loaded { ids, entries } => ids.binary_search(id).ok().map(|i| entries[i]),
            IndexKind::Chunked(c) => {
                if !c.bloom.may_contain(id) {
                    return None;
                }
                let records = c.chunk(c.chunk_for(id)?);
                records.binary_search_by_key(id, |(i, _)| *i).ok().map(|i| records[i].1)
            }
        }
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Every entry in ID order.
    pub fn iter(&self) -> Box<dyn Iterator<Item = (Ulid, IndexEntry)> + Send + '_> {
        self.after(None)
    }

    /// Entries with IDs after `after` (all when `None`), in ID order.
    pub fn after(&self, after: Option<Ulid>) -> Box<dyn Iterator<Item = (Ulid, IndexEntry)> + Send + '_> {
        match &self.kind {
            IndexKind::Loaded { ids, entries } => {
                let start = after.map_or(0, |a| ids.partition_point(|id| *id <= a));
                Box::new(ids[start..].iter().copied().zip(entries[start..].iter().copied()))
            }
            IndexKind::Chunked(c) => {
                let first = after.and_then(|a| c.chunk_for(&a)).unwrap_or(0);
                Box::new(
                    (first..c.chunks.len())
                        .flat_map(move |n| {
                            let records = c.chunk(n);
                            (0..records.len()).map(move |i| records[i])
                        })
                        .filter(move |(id, _)| after.is_none_or(|a| *id > a)),
                )
            }
        }
    }

    /// Every entry, in ID order.
    pub fn values(&self) -> impl Iterator<Item = IndexEntry> + '_ {
        self.iter().map(|(_, e)| e)
    }
}

/// An open SSTable: its path, in-memory index, and an open handle for reads.
#[derive(Debug)]
pub struct SstFile {
    pub path: PathBuf,
    pub max_seq: u64,
    pub created: i64,
    pub size_bytes: u64,
    pub index: SstIndex,
    /// Kept open: entries are read at their offsets without reopening the file.
    handle: File,
    /// ID of the key the bodies are encrypted with; `None` for unencrypted (version 2) files.
    pub key_id: Option<u64>,
    /// File format version (2, 3 or 4).
    pub version: u16,
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

        let mut index: Vec<(Ulid, IndexEntry)> = Vec::with_capacity(entries.len());
        let mut offset = HEADER_LEN;
        for entry in &entries {
            let body = match &entry.data {
                Some(data) if encrypt => keys.encrypt(&encode_all(&data[..], compression_level)?, &body_aad(&entry.id, entry.seq))?,
                Some(data) => encode_all(&data[..], compression_level)?,
                None => Vec::new(),
            };
            let record = IndexEntry { seq: entry.seq, ttl: entry.ttl, tombstone: entry.data.is_none(), offset, len: body.len() as u32 };
            if encrypt {
                // Version 4: bodies only; everything else is in the encrypted index.
                file.write_all(&body)?;
                offset += body.len() as u64;
            } else {
                // Version 2: a plaintext entry header before each body.
                let mut header = Vec::with_capacity(37);
                header.write_all(&entry.id.to_bytes())?;
                header.write_u8(if record.ttl.is_some() { FLAG_TTL } else { 0 } | if record.tombstone { FLAG_TOMBSTONE } else { 0 })?;
                header.write_u64::<BigEndian>(entry.seq)?;
                if let Some(ttl) = entry.ttl {
                    header.write_i64::<BigEndian>(ttl)?;
                }
                header.write_u32::<BigEndian>(body.len() as u32)?;
                file.write_all(&header)?;
                file.write_all(&body)?;
                offset += (header.len() + body.len()) as u64;
            }
            index.push((entry.id, record));
        }

        let mut index_block = Vec::new();
        if encrypt {
            for (n, chunk) in index.chunks(CHUNK_RECORDS).enumerate() {
                let mut plain = Vec::with_capacity(chunk.len() * 45);
                for (id, entry) in chunk {
                    write_record(&mut plain, id, entry)?;
                }
                let sealed = keys.encrypt(&plain, &chunk_aad(n))?;
                index_block.write_u32::<BigEndian>(sealed.len() as u32)?;
                index_block.write_all(&sealed)?;
            }
        } else {
            for (id, entry) in &index {
                write_record(&mut index_block, id, entry)?;
            }
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
        if ![LEGACY_VERSION, V3, VERSION].contains(&version) {
            bail!(
                "{} is SSTable version {}, but this HexDB reads versions {} to {}. Move files from other HexDB builds out of the data directory.",
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

        let handle = file.into_inner();
        let index = if version == VERSION {
            let key_id = key_id.ok_or_else(|| anyhow!("{} is version 4 but not encrypted", path.display()))?;
            // Read every chunk once: verifies it, and builds the directory and Bloom filter.
            let mut chunks = Vec::new();
            let mut bloom = Bloom::new(entry_count as usize);
            let mut cursor = Cursor::new(&index_block[..]);
            let mut seen = 0usize;
            let mut n = 0usize;
            while (cursor.position() as usize) < index_block.len() {
                let len = cursor.read_u32::<BigEndian>()?;
                let start = cursor.position() as usize;
                let end = start + len as usize;
                if end > index_block.len() {
                    bail!("{} is corrupt (index chunk outside the index)", path.display());
                }
                let plain = keys
                    .decrypt(key_id, &index_block[start..end], &chunk_aad(n))
                    .with_context(|| format!("{}: index chunk {} can't be decrypted", path.display(), n))?;
                let records = parse_records(&plain, None)?;
                let Some((first, _)) = records.first() else { bail!("{} has an empty index chunk", path.display()) };
                chunks.push((*first, index_offset + start as u64, len));
                for (id, _) in &records {
                    bloom.insert(id);
                }
                seen += records.len();
                cursor.set_position(end as u64);
                n += 1;
            }
            if seen as u64 != entry_count {
                bail!("{} is corrupt (index has {} entries, header says {})", path.display(), seen, entry_count);
            }
            SstIndex {
                count: seen,
                kind: IndexKind::Chunked(ChunkedIndex {
                    chunks,
                    bloom,
                    handle: handle.try_clone()?,
                    keys: keys.clone(),
                    key_id,
                    path: path.to_path_buf(),
                    cache: std::sync::Mutex::new(Vec::new()),
                }),
            }
        } else {
            SstIndex::loaded(parse_records(&index_block, Some(entry_count as usize))?)
        };

        Ok(SstFile { path: path.to_path_buf(), max_seq, created, size_bytes, index, handle, key_id, version, keys: keys.clone() })
    }

    /// Read, decrypt and decompress one entry's document JSON.
    pub fn read_entry(&self, id: &Ulid, entry: &IndexEntry) -> Result<Vec<u8>> {
        let compressed = if self.version == VERSION {
            let mut body = vec![0u8; entry.len as usize];
            read_exact_at(&self.handle, &mut body, entry.offset).with_context(|| format!("Failed to read {}", self.path.display()))?;
            let key_id = self.key_id.unwrap_or_default();
            self.keys
                .decrypt(key_id, &body, &body_aad(id, entry.seq))
                .with_context(|| format!("{}: document {} can't be decrypted", self.path.display(), id))?
        } else {
            // Versions 2 and 3: an entry header precedes the body.
            let header_len = 16 + 1 + 8 + if entry.ttl.is_some() { 8 } else { 0 } + 4;
            let mut buf = vec![0u8; header_len + entry.len as usize];
            read_exact_at(&self.handle, &mut buf, entry.offset).with_context(|| format!("Failed to read {}", self.path.display()))?;
            let stored_id = Ulid::from_bytes(buf[..16].try_into().unwrap());
            let len = u32::from_be_bytes(buf[header_len - 4..header_len].try_into().unwrap());
            if len != entry.len || stored_id != *id {
                bail!("{} is corrupt (entry header mismatch)", self.path.display());
            }
            let body = buf.split_off(header_len);
            match self.key_id {
                Some(key_id) => self
                    .keys
                    .decrypt(key_id, &body, &body_aad(id, entry.seq))
                    .with_context(|| format!("{}: document {} can't be decrypted", self.path.display(), id))?,
                None => body,
            }
        };
        Ok(decode_all(&compressed[..])?)
    }
}

/// Read exactly `buf.len()` bytes at `offset`, without moving a shared cursor
/// (so concurrent reads of one file don't interfere).
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::read_at(file, buf, offset)?;
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_read(file, buf, offset)?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "unexpected end of file"));
        }
        buf = &mut buf[n..];
        offset += n as u64;
    }
    Ok(())
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
            .filter_map(|f| f.index.get(id).map(|e| (f, e)))
            .max_by_key(|(_, e)| e.seq)?;
        Some(if entry.tombstone {
            DiskLookup::Tombstone { seq: entry.seq }
        } else {
            DiskLookup::Live { file: file.clone(), entry }
        })
    }

    /// Documents of a tessellation with an on-disk version newer than `seq`.
    pub async fn ids_changed_after(&self, tess: &str, seq: u64) -> HashSet<Ulid> {
        let tables = self.tables.read().await;
        let mut ids = HashSet::new();
        for file in tables.get(tess).into_iter().flatten().filter(|f| f.max_seq > seq) {
            ids.extend(file.index.iter().filter(|(_, e)| e.seq > seq).map(|(id, _)| id));
        }
        ids
    }

    /// Newest on-disk index entry for every document in a tessellation.
    pub async fn latest_entries(&self, tess: &str) -> HashMap<Ulid, (Arc<SstFile>, IndexEntry)> {
        let tables = self.tables.read().await;
        let mut latest: HashMap<Ulid, (Arc<SstFile>, IndexEntry)> = HashMap::new();
        for file in tables.get(tess).into_iter().flatten() {
            for (id, entry) in file.index.iter() {
                match latest.get(&id) {
                    Some((_, existing)) if existing.seq >= entry.seq => {}
                    _ => {
                        latest.insert(id, (file.clone(), entry));
                    }
                }
            }
        }
        latest
    }

    /// Up to `limit` on-disk entries of a tessellation after `after`, in ID
    /// order, newest version per ID (tombstones included).
    pub async fn entries_after(&self, tess: &str, after: Option<Ulid>, limit: usize) -> Vec<(Ulid, IndexEntry)> {
        let tables = self.tables.read().await;
        let Some(files) = tables.get(tess) else { return Vec::new() };
        // Take `limit` from each file, merge, and keep the newest per ID: the
        // first `limit` IDs of the merge are complete.
        let mut merged: BTreeMap<Ulid, IndexEntry> = BTreeMap::new();
        for file in files {
            for (id, entry) in file.index.after(after).take(limit) {
                match merged.get(&id) {
                    Some(existing) if existing.seq >= entry.seq => {}
                    _ => {
                        merged.insert(id, entry);
                    }
                }
            }
        }
        merged.into_iter().take(limit).collect()
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
        self.tables.read().await.values().flatten().filter(|f| f.key_id != current || f.version != VERSION).count()
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
                for (id, entry) in file.index.iter() {
                    total_entries += 1;
                    match latest.get(&id) {
                        Some((_, existing)) if existing.seq >= entry.seq => {}
                        _ => {
                            latest.insert(id, (file.clone(), entry));
                        }
                    }
                }
            }

            let droppable = |e: &IndexEntry| (e.tombstone || e.is_expired(now_millis)) && e.seq < drop_floor;
            let drop_count = latest.values().filter(|(_, e)| droppable(e)).count();
            // Rewrite files that aren't under the current key (key rotation, or
            // files from before encryption).
            let stale_key = files.iter().any(|f| f.key_id != Some(self.keys.current_id()) || f.version != VERSION);
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
                    let data = if entry.tombstone { None } else { Some(file.read_entry(&id, &entry)?) };
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

    /// A version 3 file (encrypted bodies, plaintext entry headers and index), as earlier builds wrote.
    fn write_v3(path: &Path, entries: &[(Ulid, u64, &[u8])], keys: &KeyRing) {
        let mut out = vec![0u8; HEADER_LEN as usize];
        let mut index = Vec::new();
        for (id, seq, data) in entries {
            let body = keys.encrypt(&encode_all(*data, 0).unwrap(), &body_aad(id, *seq)).unwrap();
            let offset = out.len() as u64;
            out.extend_from_slice(&id.to_bytes());
            out.push(0);
            out.extend_from_slice(&seq.to_be_bytes());
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            out.extend_from_slice(&body);
            write_record(&mut index, id, &IndexEntry { seq: *seq, ttl: None, tombstone: false, offset, len: body.len() as u32 }).unwrap();
        }
        let index_offset = out.len() as u64;
        out.extend_from_slice(&index);
        let mut header = Vec::new();
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&V3.to_be_bytes());
        header.push(COMPRESSION_ZSTD);
        header.push(ENCRYPTION_AES_GCM);
        header.extend_from_slice(&(entries.len() as u64).to_be_bytes());
        header.extend_from_slice(&0i64.to_be_bytes());
        header.extend_from_slice(&index_offset.to_be_bytes());
        header.extend_from_slice(&(index.len() as u64).to_be_bytes());
        header.extend_from_slice(&checksum(&index).to_be_bytes());
        header.extend_from_slice(&entries.iter().map(|e| e.1).max().unwrap().to_be_bytes());
        header.extend_from_slice(&keys.current_id().to_be_bytes());
        out[..HEADER_LEN as usize].copy_from_slice(&header);
        fs::write(path, out).unwrap();
    }

    #[test]
    fn version_4_index_is_chunked_encrypted_and_complete() {
        let dir = temp_dir();
        let path = dir.join("big.hxs");
        let mut ids: Vec<Ulid> = (0..1000).map(|_| Ulid::new()).collect();
        ids.sort();
        let entries: Vec<SstEntry> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| SstEntry { id: *id, seq: i as u64 + 1, ttl: (i % 5 == 0).then_some(42), data: (i % 7 != 0).then(|| format!("{{\"n\":{}}}", i).into_bytes()) })
            .collect();
        let file = SstFile::write(&path, entries, 0, 0, &test_keys()).unwrap();
        assert_eq!(file.version, VERSION);
        assert_eq!(file.index.len(), 1000);
        // No document ID appears in the file in plaintext.
        let raw = fs::read(&path).unwrap();
        for id in ids.iter().step_by(50) {
            assert!(!raw.windows(16).any(|w| w == id.to_bytes()), "ID in plaintext");
        }
        // Every ID is found (no Bloom false negatives); unknown IDs are not.
        for (i, id) in ids.iter().enumerate() {
            let e = file.index.get(id).unwrap();
            assert_eq!(e.seq, i as u64 + 1);
            assert_eq!(e.tombstone, i % 7 == 0);
            assert_eq!(e.ttl, (i % 5 == 0).then_some(42));
            if !e.tombstone {
                assert_eq!(file.read_entry(id, &e).unwrap(), format!("{{\"n\":{}}}", i).into_bytes());
            }
        }
        assert!((0..200).all(|_| file.index.get(&Ulid::new()).is_none()));
        // Ordered iteration, from the start and from any cursor, across chunks.
        assert_eq!(file.index.iter().map(|(id, _)| id).collect::<Vec<_>>(), ids);
        for cut in [0usize, 63, 64, 65, 500, 998, 999] {
            let after: Vec<Ulid> = file.index.after(Some(ids[cut])).map(|(id, _)| id).collect();
            assert_eq!(after, ids[cut + 1..].to_vec(), "after {}", cut);
        }
        // Reopening rebuilds the directory and filter.
        let reopened = SstFile::open(&path, &test_keys()).unwrap();
        assert_eq!(reopened.index.get(&ids[777]).unwrap().seq, 778);
        // A wrong key can't open it.
        assert!(SstFile::open(&path, &Arc::new(KeyRing::new(&[8u8; 32], &[]))).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn version_3_files_are_read_and_compacted_to_version_4() {
        let dir = temp_dir();
        let tess_dir = dir.join("t");
        fs::create_dir_all(&tess_dir).unwrap();
        let keys = test_keys();
        let (a, b) = (Ulid::new(), Ulid::new());
        write_v3(&tess_dir.join("old.hxs"), &[(a, 1, b"{\"v\":1}"), (b, 2, b"{\"v\":2}")], &keys);
        let store = SstStore::open(&dir, 0, keys.clone()).unwrap();
        let Some(DiskLookup::Live { file, entry }) = store.lookup("t", &a).await else { panic!() };
        assert_eq!(file.version, V3);
        assert_eq!(file.read_entry(&a, &entry).unwrap(), b"{\"v\":1}");
        assert_eq!(store.files_needing_rewrite().await, 1);
        store.compact(0, 0).await.unwrap();
        assert_eq!(store.files_needing_rewrite().await, 0);
        let Some(DiskLookup::Live { file, entry }) = store.lookup("t", &b).await else { panic!() };
        assert_eq!(file.version, VERSION);
        assert_eq!(file.read_entry(&b, &entry).unwrap(), b"{\"v\":2}");
        fs::remove_dir_all(&dir).ok();
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
        let ea = file.index.get(&a).unwrap();
        assert_eq!((ea.seq, ea.ttl, ea.tombstone), (5, Some(99), false));
        assert_eq!(file.read_entry(&a, &ea).unwrap(), b"{\"a\":1}");
        assert!(file.index.get(&b).unwrap().tombstone);

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
        let (ea, eb) = (file.index.get(&a).unwrap(), file.index.get(&b).unwrap());
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
        assert!(reopened.read_entry(&a, &reopened.index.get(&a).unwrap()).is_err());
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
            assert!(file.read_entry(&id, &entry).is_ok());
        }
        store.compact(0, 0).await.unwrap();
        assert_eq!(store.files_needing_rewrite().await, 0);
        drop(store);

        let store = SstStore::open(&dir, 0, Arc::new(KeyRing::new(&new_key, &[]))).unwrap();
        let Some(DiskLookup::Live { file, entry }) = store.lookup("t", &b).await else { panic!() };
        assert_eq!(file.read_entry(&b, &entry).unwrap(), b"{\"v\":\"b\"}");
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
