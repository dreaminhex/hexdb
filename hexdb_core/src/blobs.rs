// HexDB Core Chunk Store
//
// File content (see `crate::files`) lives here, outside the WAL, the SSTables
// and the in-memory documents. A file is cut into content-defined chunks, and
// each chunk is stored once however many files (or versions) contain it.
//
// Storing a chunk:
//   1. Its ID is a keyed BLAKE3 hash of its plaintext, with the content key
//      kept in the catalog, so equal content is stored once but an ID doesn't
//      reveal the plain hash of a known file.
//   2. It's compressed with Zstandard when the caller asks and that saves at
//      least 5%.
//   3. It's encrypted with AES-256-GCM under the storage key, with its ID and
//      flags as authenticated data.
//   4. The ciphertext is split with the hex's Reed-Solomon code into four data
//      and two parity shards, and shard n is appended to vertex n's pack file
//      (`<vertex dir>/<number>.hxp`) with a BLAKE3 hash of the shard.
//
// Any two of a chunk's six shards can be missing or corrupt: a read rebuilds
// the chunk from the other four and writes the rebuilt shards back. A scrub
// verifies every shard. Vertex directories can be on different disks.
//
// Pack files are append-only. The in-memory index (chunk ID -> where each
// shard is) is rebuilt at startup from the record headers. A torn record at
// the end of a vertex's newest pack (a crash mid-append) is cut off.
//
// Unreferenced chunks are removed by mark and sweep: the caller starts a mark
// (`begin_mark`), collects every chunk its documents reference, and calls
// `sweep`. Chunks written, reused or released after the mark began (by
// generation) and chunks pinned by uploads in progress are kept. Packs that are
// mostly dead are rewritten, and chunks under a previous key are re-encrypted.

use crate::crypt::KeyRing;
use crate::hex::{DATA_SHARDS, VERTEX_COUNT};
use crate::sst::read_exact_at;
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tracing::{info, warn};

/// A chunk's ID: keyed BLAKE3 of its plaintext.
pub type ChunkId = [u8; 32];

/// Content-defined chunking bounds. An average of 2 MB keeps the in-memory
/// index to about 80 MB per TB stored while still sharing unchanged regions
/// between versions of large files.
pub const MIN_CHUNK: usize = 512 * 1024;
pub const AVG_CHUNK: usize = 2 * 1024 * 1024;
pub const MAX_CHUNK: usize = 8 * 1024 * 1024;

const PACK_MAGIC: &[u8; 4] = b"HXP1";
const PACK_VERSION: u16 = 1;
const PACK_HEADER_LEN: u64 = 8;
const RECORD_MAGIC: &[u8; 4] = b"SHRD";
const RECORD_HEADER_LEN: usize = 98;
/// A pack is closed and a new one started once it reaches this size.
const MAX_PACK_BYTES: u64 = 256 * 1024 * 1024;
const FLAG_ZSTD: u8 = 1;
const NO_PACK: u32 = u32::MAX;
const MAX_OPEN_PACKS: usize = 256;

/// A chunk ID as text.
pub fn chunk_hex(id: &ChunkId) -> String {
    hex::encode(id)
}

/// Parse a chunk ID written by [`chunk_hex`].
pub fn parse_chunk_id(text: &str) -> Option<ChunkId> {
    hex::decode(text).ok()?.try_into().ok()
}

/// Bytes in each of a chunk's six shards (as `hex::encode` splits it).
fn shard_len(cipher_len: usize) -> usize {
    cipher_len.div_ceil(DATA_SHARDS).max(1).next_multiple_of(2)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ShardLoc {
    pack: u32,
    /// Offset of the record header.
    offset: u64,
}

impl ShardLoc {
    const NONE: ShardLoc = ShardLoc { pack: NO_PACK, offset: 0 };

    fn is_some(&self) -> bool {
        self.pack != NO_PACK
    }
}

#[derive(Clone, Debug)]
struct Entry {
    cipher_len: u32,
    plain_len: u32,
    flags: u8,
    key_id: u64,
    shards: [ShardLoc; VERTEX_COUNT],
    /// When the chunk was last written, reused or released (see `sweep`).
    generation: u64,
}

impl Entry {
    fn shard_len(&self) -> usize {
        shard_len(self.cipher_len as usize)
    }

    fn present(&self) -> usize {
        self.shards.iter().filter(|s| s.is_some()).count()
    }

    fn same_encoding(&self, other: &Entry) -> bool {
        self.cipher_len == other.cipher_len && self.flags == other.flags && self.key_id == other.key_id && self.plain_len == other.plain_len
    }
}

/// The header in front of every shard in a pack.
struct RecordHeader {
    id: ChunkId,
    index: u8,
    flags: u8,
    key_id: u64,
    plain_len: u32,
    cipher_len: u32,
    shard_len: u32,
    hash: [u8; 32],
}

impl RecordHeader {
    fn for_shard(id: &ChunkId, index: usize, entry: &Entry, shard: &[u8]) -> RecordHeader {
        RecordHeader {
            id: *id,
            index: index as u8,
            flags: entry.flags,
            key_id: entry.key_id,
            plain_len: entry.plain_len,
            cipher_len: entry.cipher_len,
            shard_len: shard.len() as u32,
            hash: *blake3::hash(shard).as_bytes(),
        }
    }

    /// `magic | id | index | flags | key ID | plain length | cipher length |
    /// shard length | shard hash | check`, where the check is the first 8
    /// bytes of BLAKE3 over everything before it.
    fn encode(&self) -> [u8; RECORD_HEADER_LEN] {
        let mut b = [0u8; RECORD_HEADER_LEN];
        b[0..4].copy_from_slice(RECORD_MAGIC);
        b[4..36].copy_from_slice(&self.id);
        b[36] = self.index;
        b[37] = self.flags;
        b[38..46].copy_from_slice(&self.key_id.to_be_bytes());
        b[46..50].copy_from_slice(&self.plain_len.to_be_bytes());
        b[50..54].copy_from_slice(&self.cipher_len.to_be_bytes());
        b[54..58].copy_from_slice(&self.shard_len.to_be_bytes());
        b[58..90].copy_from_slice(&self.hash);
        let check = blake3::hash(&b[..90]);
        b[90..98].copy_from_slice(&check.as_bytes()[..8]);
        b
    }

    fn decode(b: &[u8]) -> Option<RecordHeader> {
        if b.len() < RECORD_HEADER_LEN || &b[0..4] != RECORD_MAGIC || blake3::hash(&b[..90]).as_bytes()[..8] != b[90..98] {
            return None;
        }
        let u32_at = |i: usize| u32::from_be_bytes(b[i..i + 4].try_into().unwrap());
        Some(RecordHeader {
            id: b[4..36].try_into().unwrap(),
            index: b[36],
            flags: b[37],
            key_id: u64::from_be_bytes(b[38..46].try_into().unwrap()),
            plain_len: u32_at(46),
            cipher_len: u32_at(50),
            shard_len: u32_at(54),
            hash: b[58..90].try_into().unwrap(),
        })
    }

    fn entry(&self) -> Entry {
        Entry {
            cipher_len: self.cipher_len,
            plain_len: self.plain_len,
            flags: self.flags,
            key_id: self.key_id,
            shards: [ShardLoc::NONE; VERTEX_COUNT],
            generation: 0,
        }
    }
}

struct ActivePack {
    number: u32,
    file: File,
    size: u64,
    dirty: bool,
}

/// One vertex's pack files.
struct VertexPacks {
    dir: PathBuf,
    /// Pack number -> size in bytes.
    packs: BTreeMap<u32, u64>,
    active: Option<ActivePack>,
}

#[derive(Default)]
struct VertexCounters {
    corrupt_found: AtomicU64,
    repaired: AtomicU64,
}

/// Recently read chunks, least recently used evicted first.
struct ChunkCache {
    budget: usize,
    bytes: usize,
    tick: u64,
    entries: HashMap<ChunkId, (Arc<Vec<u8>>, u64)>,
}

impl ChunkCache {
    fn get(&mut self, id: &ChunkId) -> Option<Arc<Vec<u8>>> {
        self.tick += 1;
        let tick = self.tick;
        self.entries.get_mut(id).map(|(data, used)| {
            *used = tick;
            data.clone()
        })
    }

    fn put(&mut self, id: ChunkId, data: Arc<Vec<u8>>) {
        if data.len() > self.budget / 4 || self.entries.contains_key(&id) {
            return;
        }
        self.tick += 1;
        self.bytes += data.len();
        self.entries.insert(id, (data, self.tick));
        while self.bytes > self.budget {
            let Some(oldest) = self.entries.iter().min_by_key(|(_, (_, used))| *used).map(|(id, _)| *id) else { break };
            if let Some((data, _)) = self.entries.remove(&oldest) {
                self.bytes -= data.len();
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

/// Outcome of storing a chunk.
#[derive(Debug, Clone, Copy)]
pub struct Stored {
    pub id: ChunkId,
    /// False if the chunk was already stored.
    pub new: bool,
    /// Bytes added to the vertices (all six shards with headers).
    pub bytes: u64,
}

/// Statistics for `/status`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BlobStats {
    pub chunks: usize,
    /// Plaintext bytes of all stored chunks (each counted once).
    pub content_bytes: u64,
    /// Bytes in pack files across the six vertices, dead records included.
    pub stored_bytes: u64,
    pub pack_files: usize,
    /// Chunks with fewer than six good shard locations known.
    pub degraded_chunks: usize,
    /// Chunks encrypted with a previous storage key.
    pub chunks_on_old_keys: usize,
    /// Chunks copied from other hexes since startup.
    pub fetched_from_peers: u64,
    /// Chunks that couldn't be copied from any hex since startup.
    pub fetch_failures: u64,
    pub vertices: Vec<BlobVertexStats>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct BlobVertexStats {
    pub id: usize,
    pub dir: String,
    pub bytes: u64,
    pub shards: usize,
    pub corrupt_found: u64,
    pub repaired: u64,
}

/// Outcome of a scrub.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq)]
pub struct ScrubReport {
    pub chunks: usize,
    pub corrupt_shards: usize,
    pub repaired_shards: usize,
    /// Chunks that couldn't be rebuilt here but were copied from another hex.
    pub repaired_from_peers: usize,
    pub unrecoverable: usize,
}

/// Outcome of a sweep.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq)]
pub struct SweepReport {
    pub chunks_removed: usize,
    pub chunks_reencrypted: usize,
    pub packs_rewritten: usize,
    pub packs_deleted: usize,
    pub bytes_freed: u64,
}

pub struct BlobStore {
    vertices: Mutex<Vec<VertexPacks>>,
    index: RwLock<HashMap<ChunkId, Entry>>,
    keys: Arc<KeyRing>,
    content_key: RwLock<[u8; 32]>,
    level: i32,
    generation: AtomicU64,
    pins: Mutex<HashMap<ChunkId, usize>>,
    sync_lock: Mutex<()>,
    /// Held by sweeps and backups, so a backup never sees a pack half rewritten or deleted.
    maintenance: Mutex<()>,
    handles: Mutex<HashMap<(usize, u32), Arc<File>>>,
    cache: Mutex<ChunkCache>,
    counters: [VertexCounters; VERTEX_COUNT],
}

impl BlobStore {
    /// Open the six vertex directories (created if missing) and rebuild the
    /// index from their packs.
    pub fn open(dirs: &[PathBuf], keys: Arc<KeyRing>, content_key: [u8; 32], level: i32, cache_bytes: usize) -> Result<BlobStore> {
        if dirs.len() != VERTEX_COUNT {
            bail!("The chunk store needs {} vertex directories, got {}.", VERTEX_COUNT, dirs.len());
        }
        let mut variants: HashMap<ChunkId, Vec<Entry>> = HashMap::new();
        let mut vertices = Vec::with_capacity(VERTEX_COUNT);
        let mut records = 0usize;
        for (v, dir) in dirs.iter().enumerate() {
            fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
            let mut numbers: Vec<u32> = fs::read_dir(dir)
                .with_context(|| format!("Failed to list {}", dir.display()))?
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    name.strip_suffix(".hxp")?.parse().ok()
                })
                .collect();
            numbers.sort_unstable();
            let mut packs = BTreeMap::new();
            for (i, &number) in numbers.iter().enumerate() {
                let last = i + 1 == numbers.len();
                let (size, count) = scan_pack(&pack_path(dir, number), v, number, last, &mut variants)?;
                records += count;
                packs.insert(number, size);
            }
            let active = match packs.iter().next_back() {
                Some((&number, &size)) if size < MAX_PACK_BYTES => {
                    let file = OpenOptions::new().append(true).open(pack_path(dir, number))?;
                    Some(ActivePack { number, file, size, dirty: false })
                }
                _ => None,
            };
            vertices.push(VertexPacks { dir: dir.clone(), packs, active });
        }

        // A chunk re-encrypted after a key rotation has records in two
        // encodings until the old pack is deleted: keep the best one.
        let current = keys.current_id();
        let index: HashMap<ChunkId, Entry> = variants
            .into_iter()
            .filter_map(|(id, list)| {
                list.into_iter().max_by_key(|e| (e.present() >= DATA_SHARDS, e.key_id == current, e.present())).map(|e| (id, e))
            })
            .collect();
        if !index.is_empty() {
            info!("📦 Chunk store: {} chunk(s) from {} shard record(s).", index.len(), records);
        }
        Ok(BlobStore {
            vertices: Mutex::new(vertices),
            index: RwLock::new(index),
            keys,
            content_key: RwLock::new(content_key),
            level,
            generation: AtomicU64::new(1),
            pins: Mutex::new(HashMap::new()),
            sync_lock: Mutex::new(()),
            maintenance: Mutex::new(()),
            handles: Mutex::new(HashMap::new()),
            cache: Mutex::new(ChunkCache { budget: cache_bytes, bytes: 0, tick: 0, entries: HashMap::new() }),
            counters: Default::default(),
        })
    }

    fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The ID content would have.
    pub fn chunk_id(&self, plain: &[u8]) -> ChunkId {
        *blake3::keyed_hash(&self.content_key.read().unwrap(), plain).as_bytes()
    }

    /// Name chunks with another content key (a replica adopting its lattice's
    /// key). Chunks stored under the old key no longer verify and are left
    /// for garbage collection.
    pub fn set_content_key(&self, key: [u8; 32]) {
        *self.content_key.write().unwrap() = key;
        self.cache.lock().unwrap().clear();
    }

    /// Whether a stored chunk was compressed (so a copy can be stored the same way).
    pub fn is_compressed(&self, id: &ChunkId) -> Option<bool> {
        self.index.read().unwrap().get(id).map(|e| e.flags & FLAG_ZSTD != 0)
    }

    pub fn contains(&self, id: &ChunkId) -> bool {
        self.index.read().unwrap().get(id).is_some_and(|e| e.present() >= DATA_SHARDS)
    }

    /// Plaintext length of a stored chunk.
    pub fn chunk_len(&self, id: &ChunkId) -> Option<u64> {
        self.index.read().unwrap().get(id).map(|e| e.plain_len as u64)
    }

    /// Store a chunk (or reuse the stored copy) and pin it until `pins` is
    /// dropped. Not durable until [`BlobStore::sync`].
    pub fn put(&self, plain: &[u8], compress: bool, pins: &mut Pins) -> Result<Stored> {
        let id = self.chunk_id(plain);
        if self.reuse(&id, pins) {
            return Ok(Stored { id, new: false, bytes: 0 });
        }
        let bytes = self.write_chunk(&id, plain, compress, None)?;
        self.pin_locked(&id, pins);
        Ok(Stored { id, new: true, bytes })
    }

    /// Pin a chunk that's already stored (until `pins` is dropped).
    pub fn pin_existing(&self, id: &ChunkId, pins: &mut Pins) -> bool {
        self.reuse(id, pins)
    }

    /// Pin an existing chunk and mark it in use; false if it isn't stored.
    fn reuse(&self, id: &ChunkId, pins: &mut Pins) -> bool {
        let mut index = self.index.write().unwrap();
        let Some(entry) = index.get_mut(id) else { return false };
        if entry.present() < DATA_SHARDS {
            return false;
        }
        entry.generation = self.next_generation();
        drop(index);
        self.pin_locked(id, pins);
        true
    }

    fn pin_locked(&self, id: &ChunkId, pins: &mut Pins) {
        *self.pins.lock().unwrap().entry(*id).or_insert(0) += 1;
        pins.ids.push(*id);
    }

    fn unpin(&self, ids: &[ChunkId]) {
        let mut index = self.index.write().unwrap();
        let mut pins = self.pins.lock().unwrap();
        for id in ids {
            if let Some(count) = pins.get_mut(id) {
                *count -= 1;
                if *count == 0 {
                    pins.remove(id);
                }
            }
            // Released after a mark began: that mark may not have seen the
            // document that now references it.
            if let Some(entry) = index.get_mut(id) {
                entry.generation = self.next_generation();
            }
        }
    }

    /// Encrypt, shard and append a chunk; replaces any index entry. `keep`
    /// carries the generation over when re-encrypting.
    fn write_chunk(&self, id: &ChunkId, plain: &[u8], compress: bool, keep: Option<u64>) -> Result<u64> {
        let mut flags = 0u8;
        let mut body = std::borrow::Cow::Borrowed(plain);
        if compress && !plain.is_empty() {
            let packed = zstd::bulk::compress(plain, self.level).context("compressing a chunk")?;
            if packed.len() < plain.len() - plain.len() / 20 {
                body = std::borrow::Cow::Owned(packed);
                flags |= FLAG_ZSTD;
            }
        }
        let cipher = self.keys.encrypt(&body, &aad(id, flags))?;
        let mut entry = Entry {
            cipher_len: u32::try_from(cipher.len()).map_err(|_| anyhow!("chunk too large"))?,
            plain_len: plain.len() as u32,
            flags,
            key_id: self.keys.current_id(),
            shards: [ShardLoc::NONE; VERTEX_COUNT],
            generation: 0,
        };
        let shards = crate::hex::encode(&cipher);
        let mut written = 0u64;
        {
            let mut vertices = self.vertices.lock().unwrap();
            for (v, shard) in shards.iter().enumerate() {
                let header = RecordHeader::for_shard(id, v, &entry, shard);
                entry.shards[v] = append(&mut vertices[v], v, &header, shard)?;
                written += (RECORD_HEADER_LEN + shard.len()) as u64;
            }
        }
        entry.generation = keep.unwrap_or_else(|| self.next_generation());
        self.index.write().unwrap().insert(*id, entry);
        Ok(written)
    }

    /// Make everything appended so far durable.
    pub fn sync(&self) -> Result<()> {
        let _serial = self.sync_lock.lock().unwrap();
        let files = {
            let mut vertices = self.vertices.lock().unwrap();
            let mut files = Vec::new();
            for vertex in vertices.iter_mut() {
                if let Some(active) = vertex.active.as_mut().filter(|a| a.dirty) {
                    files.push(active.file.try_clone()?);
                    active.dirty = false;
                }
            }
            files
        };
        for file in files {
            file.sync_data().context("syncing a pack file")?;
        }
        Ok(())
    }

    /// Read a chunk's plaintext, rebuilding and repairing missing or corrupt shards.
    pub fn get(&self, id: &ChunkId) -> Result<Arc<Vec<u8>>> {
        if let Some(hit) = self.cache.lock().unwrap().get(id) {
            return Ok(hit);
        }
        let mut last_error = None;
        // A pack can be rewritten between looking up a chunk and reading it; the second try sees the new location.
        for _ in 0..2 {
            let entry = self.index.read().unwrap().get(id).cloned().ok_or_else(|| anyhow!("chunk {} is not stored on this hex", chunk_hex(id)))?;
            match self.read_entry(id, &entry) {
                Ok(plain) => {
                    let plain = Arc::new(plain);
                    self.cache.lock().unwrap().put(*id, plain.clone());
                    return Ok(plain);
                }
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.unwrap())
    }

    fn read_entry(&self, id: &ChunkId, entry: &Entry) -> Result<Vec<u8>> {
        let len = entry.shard_len();
        let mut shards: Vec<Option<Vec<u8>>> = vec![None; VERTEX_COUNT];
        let mut bad = Vec::new();
        for (v, slot) in shards.iter_mut().enumerate() {
            if v >= DATA_SHARDS && bad.is_empty() {
                break;
            }
            match self.read_shard(v, entry.shards[v], id, entry, len) {
                Ok(shard) => *slot = Some(shard),
                Err(_) => bad.push(v),
            }
        }
        let present = shards.iter().filter(|s| s.is_some()).count();
        if present < DATA_SHARDS {
            bail!("chunk {} can't be rebuilt: only {} of {} shards are readable", chunk_hex(id), present, VERTEX_COUNT);
        }
        if !bad.is_empty() {
            crate::hex::reconstruct(&mut shards).map_err(|_| anyhow!("chunk {} can't be rebuilt", chunk_hex(id)))?;
            for &v in &bad {
                if let Some(shard) = &shards[v] {
                    if let Err(e) = self.repair_shard(id, entry, v, shard) {
                        warn!("⚠️ Couldn't repair shard {} of chunk {}: {:#}", v, chunk_hex(id), e);
                    }
                }
            }
        }
        let mut cipher = Vec::with_capacity(len * DATA_SHARDS);
        for shard in shards.iter().take(DATA_SHARDS) {
            cipher.extend_from_slice(shard.as_deref().unwrap_or_default());
        }
        cipher.truncate(entry.cipher_len as usize);
        let body = self
            .keys
            .decrypt(entry.key_id, &cipher, &aad(id, entry.flags))
            .with_context(|| format!("chunk {} could not be decrypted", chunk_hex(id)))?;
        let plain = if entry.flags & FLAG_ZSTD != 0 {
            zstd::bulk::decompress(&body, entry.plain_len as usize).context("decompressing a chunk")?
        } else {
            body
        };
        if self.chunk_id(&plain) != *id {
            bail!("chunk {} doesn't match its ID", chunk_hex(id));
        }
        Ok(plain)
    }

    /// Read and verify one shard.
    fn read_shard(&self, v: usize, loc: ShardLoc, id: &ChunkId, entry: &Entry, len: usize) -> Result<Vec<u8>> {
        if !loc.is_some() {
            bail!("missing");
        }
        let file = self.handle(v, loc.pack)?;
        let mut buf = vec![0u8; RECORD_HEADER_LEN + len];
        let result = read_exact_at(&file, &mut buf, loc.offset).map_err(anyhow::Error::from).and_then(|_| {
            let header = RecordHeader::decode(&buf).ok_or_else(|| anyhow!("bad record header"))?;
            if header.id != *id || header.index as usize != v || header.shard_len as usize != len || header.key_id != entry.key_id {
                bail!("record doesn't belong to this chunk");
            }
            if *blake3::hash(&buf[RECORD_HEADER_LEN..]).as_bytes() != header.hash {
                bail!("shard hash mismatch");
            }
            Ok(())
        });
        if let Err(e) = result {
            self.counters[v].corrupt_found.fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }
        buf.drain(..RECORD_HEADER_LEN);
        Ok(buf)
    }

    /// Append a rebuilt shard and point the index at it (unless the chunk changed meanwhile).
    fn repair_shard(&self, id: &ChunkId, entry: &Entry, v: usize, shard: &[u8]) -> Result<()> {
        let header = RecordHeader::for_shard(id, v, entry, shard);
        let loc = {
            let mut vertices = self.vertices.lock().unwrap();
            append(&mut vertices[v], v, &header, shard)?
        };
        let mut index = self.index.write().unwrap();
        if let Some(current) = index.get_mut(id).filter(|c| c.same_encoding(entry)) {
            current.shards[v] = loc;
            self.counters[v].repaired.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn handle(&self, v: usize, pack: u32) -> Result<Arc<File>> {
        if let Some(file) = self.handles.lock().unwrap().get(&(v, pack)) {
            return Ok(file.clone());
        }
        let dir = self.vertices.lock().unwrap()[v].dir.clone();
        let file = Arc::new(File::open(pack_path(&dir, pack))?);
        let mut handles = self.handles.lock().unwrap();
        if handles.len() >= MAX_OPEN_PACKS {
            handles.clear();
        }
        handles.insert((v, pack), file.clone());
        Ok(file)
    }

    /// Verify every shard of every chunk and repair what can be repaired.
    /// Also returns the chunks that can't be rebuilt here.
    pub fn scrub(&self) -> (ScrubReport, Vec<ChunkId>) {
        let ids: Vec<ChunkId> = self.index.read().unwrap().keys().copied().collect();
        let mut report = ScrubReport::default();
        let mut lost = Vec::new();
        for id in ids {
            let Some(entry) = self.index.read().unwrap().get(&id).cloned() else { continue };
            report.chunks += 1;
            let len = entry.shard_len();
            let mut shards: Vec<Option<Vec<u8>>> =
                (0..VERTEX_COUNT).map(|v| self.read_shard(v, entry.shards[v], &id, &entry, len).ok()).collect();
            let bad: Vec<usize> = (0..VERTEX_COUNT).filter(|&v| shards[v].is_none()).collect();
            if bad.is_empty() {
                continue;
            }
            report.corrupt_shards += bad.len();
            if crate::hex::reconstruct(&mut shards).is_err() {
                report.unrecoverable += 1;
                lost.push(id);
                warn!("⚠️ Chunk {} can't be rebuilt here: {} of {} shards are bad.", chunk_hex(&id), bad.len(), VERTEX_COUNT);
                continue;
            }
            for v in bad {
                if let Some(shard) = &shards[v] {
                    if self.repair_shard(&id, &entry, v, shard).is_ok() {
                        report.repaired_shards += 1;
                    }
                }
            }
        }
        if report.repaired_shards > 0 {
            if let Err(e) = self.sync() {
                warn!("⚠️ Couldn't sync repaired shards: {:#}", e);
            }
        }
        (report, lost)
    }

    /// Store a chunk again from a copy (another hex's), replacing a damaged
    /// entry. The copy must match its ID.
    pub fn put_copy(&self, id: &ChunkId, plain: &[u8], compress: bool, pins: &mut Pins) -> Result<Stored> {
        if self.chunk_id(plain) != *id {
            bail!("the copy of chunk {} doesn't match its ID", chunk_hex(id));
        }
        let bytes = self.write_chunk(id, plain, compress, None)?;
        self.pin_locked(id, pins);
        self.cache.lock().unwrap().put(*id, Arc::new(plain.to_vec()));
        Ok(Stored { id: *id, new: true, bytes })
    }

    /// Start a mark: chunks touched from now on survive the next sweep.
    pub fn begin_mark(&self) -> u64 {
        self.next_generation()
    }

    /// Remove chunks not in `live` (unless pinned or touched since `mark`),
    /// re-encrypt chunks under a previous key, and rewrite or delete packs
    /// that are mostly dead.
    pub fn sweep(&self, live: &HashSet<ChunkId>, mark: u64) -> Result<SweepReport> {
        let _maintenance = self.maintenance.lock().unwrap();
        let mut report = SweepReport::default();
        {
            let mut index = self.index.write().unwrap();
            let pins = self.pins.lock().unwrap();
            let before = index.len();
            index.retain(|id, e| live.contains(id) || pins.contains_key(id) || e.generation >= mark);
            report.chunks_removed = before - index.len();
        }
        if report.chunks_removed > 0 {
            self.cache.lock().unwrap().clear();
        }

        // Chunks under a previous key: decrypt and store again under the current one.
        let current = self.keys.current_id();
        let old: Vec<(ChunkId, Entry)> =
            self.index.read().unwrap().iter().filter(|(_, e)| e.key_id != current).map(|(id, e)| (*id, e.clone())).collect();
        for (id, entry) in old {
            match self.read_entry(&id, &entry) {
                Ok(plain) => {
                    self.write_chunk(&id, &plain, entry.flags & FLAG_ZSTD != 0, Some(entry.generation))?;
                    report.chunks_reencrypted += 1;
                }
                Err(e) => warn!("⚠️ Couldn't re-encrypt chunk {}: {:#}", chunk_hex(&id), e),
            }
        }

        // Move live shards out of packs that are mostly dead.
        let live_bytes = self.live_bytes();
        let candidates: Vec<(usize, u32)> = {
            let vertices = self.vertices.lock().unwrap();
            let mut list = Vec::new();
            for (v, vertex) in vertices.iter().enumerate() {
                let active = vertex.active.as_ref().map(|a| a.number);
                for (&number, &size) in &vertex.packs {
                    let used = live_bytes.get(&(v, number)).copied().unwrap_or(0);
                    if Some(number) != active && used > 0 && used * 2 < size.saturating_sub(PACK_HEADER_LEN) {
                        list.push((v, number));
                    }
                }
            }
            list
        };
        for (v, number) in candidates {
            let movers: Vec<(ChunkId, Entry)> = self
                .index
                .read()
                .unwrap()
                .iter()
                .filter(|(_, e)| e.shards[v].pack == number)
                .map(|(id, e)| (*id, e.clone()))
                .collect();
            for (id, entry) in movers {
                let len = entry.shard_len();
                match self.read_shard(v, entry.shards[v], &id, &entry, len) {
                    Ok(shard) => self.repair_shard(&id, &entry, v, &shard)?,
                    // A bad shard here: rebuilding the chunk repairs it into the active pack.
                    Err(_) => {
                        let _ = self.read_entry(&id, &entry);
                    }
                }
            }
            report.packs_rewritten += 1;
        }
        self.sync()?;

        // Delete packs no chunk points into any more.
        let live_bytes = self.live_bytes();
        let mut doomed = Vec::new();
        {
            let mut vertices = self.vertices.lock().unwrap();
            for (v, vertex) in vertices.iter_mut().enumerate() {
                let active = vertex.active.as_ref().map(|a| a.number);
                let dead: Vec<u32> = vertex
                    .packs
                    .keys()
                    .copied()
                    .filter(|n| Some(*n) != active && !live_bytes.contains_key(&(v, *n)))
                    .collect();
                for number in dead {
                    if let Some(size) = vertex.packs.remove(&number) {
                        report.bytes_freed += size;
                    }
                    doomed.push((v, pack_path(&vertex.dir, number)));
                    self.handles.lock().unwrap().remove(&(v, number));
                }
            }
        }
        for (_, path) in &doomed {
            if let Err(e) = fs::remove_file(path) {
                warn!("⚠️ Couldn't delete {}: {}", path.display(), e);
            }
        }
        report.packs_deleted = doomed.len();
        Ok(report)
    }

    /// Run `f` over every pack file, as (vertex, path, bytes to copy, still
    /// being appended), with sweeps held off. Bytes appended after the call
    /// starts aren't included; sealed packs never change.
    pub fn with_snapshot<R>(&self, f: impl FnOnce(&[(usize, PathBuf, u64, bool)]) -> R) -> R {
        let _maintenance = self.maintenance.lock().unwrap();
        self.sync().ok();
        let packs: Vec<(usize, PathBuf, u64, bool)> = {
            let vertices = self.vertices.lock().unwrap();
            vertices
                .iter()
                .enumerate()
                .flat_map(|(v, vertex)| {
                    let active = vertex.active.as_ref().map(|a| a.number);
                    vertex.packs.iter().map(move |(&n, &size)| (v, pack_path(&vertex.dir, n), size, Some(n) == active)).collect::<Vec<_>>()
                })
                .collect()
        };
        f(&packs)
    }

    /// Bytes of live shard records per (vertex, pack).
    fn live_bytes(&self) -> HashMap<(usize, u32), u64> {
        let mut used = HashMap::new();
        for entry in self.index.read().unwrap().values() {
            let record = (RECORD_HEADER_LEN + entry.shard_len()) as u64;
            for (v, loc) in entry.shards.iter().enumerate() {
                if loc.is_some() {
                    *used.entry((v, loc.pack)).or_insert(0) += record;
                }
            }
        }
        used
    }

    pub fn stats(&self) -> BlobStats {
        let current = self.keys.current_id();
        let mut stats = BlobStats::default();
        let mut shards = [0usize; VERTEX_COUNT];
        {
            let index = self.index.read().unwrap();
            stats.chunks = index.len();
            for entry in index.values() {
                stats.content_bytes += entry.plain_len as u64;
                if entry.present() < VERTEX_COUNT {
                    stats.degraded_chunks += 1;
                }
                if entry.key_id != current {
                    stats.chunks_on_old_keys += 1;
                }
                for (v, loc) in entry.shards.iter().enumerate() {
                    if loc.is_some() {
                        shards[v] += 1;
                    }
                }
            }
        }
        let vertices = self.vertices.lock().unwrap();
        for (v, vertex) in vertices.iter().enumerate() {
            let bytes: u64 = vertex.packs.values().sum();
            stats.stored_bytes += bytes;
            stats.pack_files += vertex.packs.len();
            stats.vertices.push(BlobVertexStats {
                id: v,
                dir: vertex.dir.display().to_string(),
                bytes,
                shards: shards[v],
                corrupt_found: self.counters[v].corrupt_found.load(Ordering::Relaxed),
                repaired: self.counters[v].repaired.load(Ordering::Relaxed),
            });
        }
        stats
    }

    /// Forget the open read handles (tests delete packs underneath the store).
    #[doc(hidden)]
    pub fn close_handles_for_testing(&self) {
        self.handles.lock().unwrap().clear();
        self.cache.lock().unwrap().clear();
    }
}

/// Chunks kept from being swept while an upload that references them is in
/// progress. Dropping it releases them.
pub struct Pins {
    store: Arc<BlobStore>,
    ids: Vec<ChunkId>,
}

impl Pins {
    pub fn new(store: Arc<BlobStore>) -> Pins {
        Pins { store, ids: Vec::new() }
    }
}

impl Drop for Pins {
    fn drop(&mut self) {
        self.store.unpin(&self.ids);
    }
}

fn aad(id: &ChunkId, flags: u8) -> Vec<u8> {
    let mut aad = Vec::with_capacity(33);
    aad.extend_from_slice(id);
    aad.push(flags);
    aad
}

fn pack_path(dir: &Path, number: u32) -> PathBuf {
    dir.join(format!("{:08}.hxp", number))
}

/// Append one shard record to a vertex's active pack, starting a new pack when needed.
fn append(vertex: &mut VertexPacks, v: usize, header: &RecordHeader, shard: &[u8]) -> Result<ShardLoc> {
    let full = vertex.active.as_ref().is_none_or(|a| a.size >= MAX_PACK_BYTES);
    if full {
        if let Some(old) = vertex.active.take() {
            if old.dirty {
                old.file.sync_data().context("syncing a full pack file")?;
            }
        }
        let number = vertex.packs.keys().next_back().map_or(1, |n| n + 1);
        let path = pack_path(&vertex.dir, number);
        let mut file = OpenOptions::new().create_new(true).append(true).open(&path).with_context(|| format!("Failed to create {}", path.display()))?;
        let mut head = Vec::with_capacity(PACK_HEADER_LEN as usize);
        head.extend_from_slice(PACK_MAGIC);
        head.extend_from_slice(&PACK_VERSION.to_be_bytes());
        head.push(v as u8);
        head.push(0);
        file.write_all(&head)?;
        file.sync_all()?;
        crate::wal::sync_dir(&vertex.dir);
        vertex.packs.insert(number, PACK_HEADER_LEN);
        vertex.active = Some(ActivePack { number, file, size: PACK_HEADER_LEN, dirty: false });
    }
    let active = vertex.active.as_mut().unwrap();
    let mut record = Vec::with_capacity(RECORD_HEADER_LEN + shard.len());
    record.extend_from_slice(&header.encode());
    record.extend_from_slice(shard);
    active.file.write_all(&record).context("appending to a pack file")?;
    let loc = ShardLoc { pack: active.number, offset: active.size };
    active.size += record.len() as u64;
    active.dirty = true;
    vertex.packs.insert(active.number, active.size);
    Ok(loc)
}

/// Read a pack's record headers into `variants`. Returns the pack's usable
/// size and its record count. A torn or corrupt record ends the scan; in the
/// newest pack (where a crash mid-append leaves one) the tail is cut off.
fn scan_pack(path: &Path, v: usize, number: u32, newest: bool, variants: &mut HashMap<ChunkId, Vec<Entry>>) -> Result<(u64, usize)> {
    let file = OpenOptions::new().read(true).write(newest).open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let len = file.metadata()?.len();
    let mut head = [0u8; PACK_HEADER_LEN as usize];
    if len < PACK_HEADER_LEN || read_exact_at(&file, &mut head, 0).is_err() || &head[..4] != PACK_MAGIC {
        if newest && len < PACK_HEADER_LEN {
            // Created but never written: rewrite the header.
            drop(file);
            let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
            file.write_all(PACK_MAGIC)?;
            file.write_all(&PACK_VERSION.to_be_bytes())?;
            file.write_all(&[v as u8, 0])?;
            file.sync_all()?;
            return Ok((PACK_HEADER_LEN, 0));
        }
        bail!("{} is not a HexDB pack file", path.display());
    }
    if u16::from_be_bytes([head[4], head[5]]) != PACK_VERSION {
        bail!("{} has an unsupported pack version", path.display());
    }
    let mut offset = PACK_HEADER_LEN;
    let mut count = 0;
    let mut header = [0u8; RECORD_HEADER_LEN];
    let end = loop {
        if offset == len {
            break None;
        }
        if offset + RECORD_HEADER_LEN as u64 > len || read_exact_at(&file, &mut header, offset).is_err() {
            break Some("an incomplete record");
        }
        let Some(record) = RecordHeader::decode(&header) else { break Some("a corrupt record header") };
        let next = offset + (RECORD_HEADER_LEN + record.shard_len as usize) as u64;
        if next > len {
            break Some("an incomplete record");
        }
        if record.index as usize == v && record.shard_len as usize == shard_len(record.cipher_len as usize) {
            let list = variants.entry(record.id).or_default();
            let fresh = record.entry();
            let slot = match list.iter().position(|e| e.same_encoding(&fresh)) {
                Some(i) => i,
                None => {
                    list.push(fresh);
                    list.len() - 1
                }
            };
            list[slot].shards[v] = ShardLoc { pack: number, offset };
            count += 1;
        }
        offset = next;
    };
    if let Some(problem) = end {
        if newest {
            warn!("⚠️ {} ends with {} at byte {} (a crash mid-write); cutting it off.", path.display(), problem, offset);
            file.set_len(offset)?;
            file.sync_all()?;
            return Ok((offset, count));
        }
        warn!(
            "⚠️ {} has {} at byte {}; shards after it are rebuilt from the other vertices when read or scrubbed.",
            path.display(),
            problem,
            offset
        );
    }
    Ok((len, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(root: &Path) -> Vec<PathBuf> {
        (0..VERTEX_COUNT).map(|v| root.join(format!("vertex-{}", v))).collect()
    }

    fn store(root: &Path, keys: &Arc<KeyRing>) -> Arc<BlobStore> {
        Arc::new(BlobStore::open(&dirs(root), keys.clone(), [9u8; 32], 3, 1 << 20).unwrap())
    }

    fn sample(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("hexdb-blobs-{}-{}", name, ulid::Ulid::new()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn stores_dedups_and_survives_reopen() {
        let root = temp_root("basic");
        let keys = Arc::new(KeyRing::new(&[1u8; 32], &[]));
        let s = store(&root, &keys);
        let data = sample(300_000, 1);
        let text = b"hello hello hello hello hello hello hello hello hello hello".repeat(1000);
        let mut pins = Pins::new(s.clone());
        let a = s.put(&data, false, &mut pins).unwrap();
        let b = s.put(&data, false, &mut pins).unwrap();
        let t = s.put(&text, true, &mut pins).unwrap();
        assert!(a.new && !b.new && a.id == b.id);
        s.sync().unwrap();
        assert_eq!(*s.get(&a.id).unwrap(), data);
        assert_eq!(*s.get(&t.id).unwrap(), text);
        let raw: Vec<u8> = fs::read(pack_path(&dirs(&root)[0], 1)).unwrap();
        assert!(!raw.windows(11).any(|w| w == b"hello hello"), "encrypted on disk");
        drop(pins);
        drop(s);

        let s = store(&root, &keys);
        assert_eq!(*s.get(&a.id).unwrap(), data);
        assert_eq!(*s.get(&t.id).unwrap(), text);
        assert_eq!(s.stats().chunks, 2);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn any_two_vertices_can_be_lost() {
        let root = temp_root("vertices");
        let keys = Arc::new(KeyRing::new(&[1u8; 32], &[]));
        let data = sample(1_000_000, 2);
        let id = {
            let s = store(&root, &keys);
            let mut pins = Pins::new(s.clone());
            let id = s.put(&data, false, &mut pins).unwrap().id;
            s.sync().unwrap();
            id
        };
        // Corrupt vertex 1's shard in place, delete vertex 4 entirely.
        let p1 = pack_path(&dirs(&root)[1], 1);
        let mut bytes = fs::read(&p1).unwrap();
        let n = bytes.len();
        bytes[n - 10] ^= 0xff;
        fs::write(&p1, bytes).unwrap();
        fs::remove_dir_all(&dirs(&root)[4]).unwrap();

        let s = store(&root, &keys);
        assert_eq!(*s.get(&id).unwrap(), data);
        s.sync().unwrap();
        let stats = s.stats();
        assert_eq!(stats.vertices[1].repaired + stats.vertices[4].repaired, 2, "{:?}", stats);
        let (report, lost) = s.scrub();
        assert_eq!(report, ScrubReport { chunks: 1, ..Default::default() }, "repairs stuck");
        assert!(lost.is_empty());

        // Three lost vertices: unreadable.
        drop(s);
        for v in [0, 2, 3] {
            fs::remove_dir_all(&dirs(&root)[v]).unwrap();
        }
        let s = store(&root, &keys);
        assert!(s.get(&id).is_err());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn torn_tail_is_cut_off() {
        let root = temp_root("torn");
        let keys = Arc::new(KeyRing::new(&[1u8; 32], &[]));
        let (first, data) = {
            let s = store(&root, &keys);
            let mut pins = Pins::new(s.clone());
            let data = sample(50_000, 3);
            let first = s.put(&data, false, &mut pins).unwrap().id;
            s.put(&sample(50_000, 4), false, &mut pins).unwrap();
            s.sync().unwrap();
            (first, data)
        };
        let p = pack_path(&dirs(&root)[0], 1);
        let len = fs::metadata(&p).unwrap().len();
        let file = OpenOptions::new().write(true).open(&p).unwrap();
        file.set_len(len - 100).unwrap();
        drop(file);
        let s = store(&root, &keys);
        assert_eq!(*s.get(&first).unwrap(), data);
        assert!(fs::metadata(&p).unwrap().len() < len - 100, "cut back to the last whole record");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn sweep_respects_marks_pins_and_rotates_keys() {
        let root = temp_root("sweep");
        let old_keys = Arc::new(KeyRing::new(&[1u8; 32], &[]));
        let (keep, drop_id, keep_data) = {
            let s = store(&root, &old_keys);
            let mut pins = Pins::new(s.clone());
            let keep_data = sample(200_000, 5);
            let keep = s.put(&keep_data, false, &mut pins).unwrap().id;
            let drop_id = s.put(&sample(200_000, 6), false, &mut pins).unwrap().id;
            s.sync().unwrap();
            (keep, drop_id, keep_data)
        };
        let keys = Arc::new(KeyRing::new(&[2u8; 32], &[[1u8; 32]]));
        let s = store(&root, &keys);
        assert_eq!(s.stats().chunks_on_old_keys, 2);

        let mark = s.begin_mark();
        // Written after the mark began: kept although not in the live set.
        let mut pins = Pins::new(s.clone());
        let fresh = s.put(&sample(10_000, 7), false, &mut pins).unwrap().id;
        drop(pins);
        let live: HashSet<ChunkId> = [keep].into_iter().collect();
        let report = s.sweep(&live, mark).unwrap();
        assert_eq!(report.chunks_removed, 1);
        assert_eq!(report.chunks_reencrypted, 1);
        assert!(s.get(&drop_id).is_err());
        assert!(s.contains(&fresh));
        assert_eq!(*s.get(&keep).unwrap(), keep_data);
        assert_eq!(s.stats().chunks_on_old_keys, 0);
        drop(s);

        // The old key is no longer needed.
        let s = store(&root, &Arc::new(KeyRing::new(&[2u8; 32], &[])));
        assert_eq!(*s.get(&keep).unwrap(), keep_data);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn pinned_chunks_survive_a_sweep() {
        let root = temp_root("pins");
        let keys = Arc::new(KeyRing::new(&[1u8; 32], &[]));
        let s = store(&root, &keys);
        let mut pins = Pins::new(s.clone());
        let id = s.put(&sample(1000, 8), false, &mut pins).unwrap().id;
        let mark = s.begin_mark();
        s.sweep(&HashSet::new(), mark).unwrap();
        assert!(s.contains(&id), "pinned");
        drop(pins);
        // Released after the mark began: still kept by that sweep.
        s.sweep(&HashSet::new(), mark).unwrap();
        assert!(s.contains(&id));
        let mark = s.begin_mark();
        s.sweep(&HashSet::new(), mark).unwrap();
        assert!(!s.contains(&id));
        fs::remove_dir_all(&root).ok();
    }
}
