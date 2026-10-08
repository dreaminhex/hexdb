// HexDB Core Hexagonal Storage
// The hex is the in-memory store (memtable) of a HexDB node. It holds every
// write that hasn't been flushed to SSTables yet, plus a cache of recently
// used documents.
//
// Each document is Reed-Solomon encoded into six equal shards, four data and
// two parity, with one shard on each of the six vertices. Any two vertices can
// lose or corrupt a document's shard and the document can still be read and
// repaired. Every entry carries the sequence number of the write that produced
// it, so the newest version always wins regardless of load order.

use crate::vertex::Vertex;
use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use ulid::Ulid;

pub const DATA_SHARDS: usize = 4;
pub const PARITY_SHARDS: usize = 2;
pub const VERTEX_COUNT: usize = DATA_SHARDS + PARITY_SHARDS;

/// Identifies a document: tessellation name plus document ID.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocKey {
    pub tessellation: String,
    pub id: Ulid,
}

impl DocKey {
    pub fn new(tessellation: &str, id: Ulid) -> Self {
        DocKey {
            tessellation: tessellation.to_string(),
            id,
        }
    }
}

/// Metadata for one entry in the hex.
#[derive(Clone, Debug)]
pub struct EntryMeta {
    /// Sequence number of the write that produced this version.
    pub seq: u64,
    /// Expiry time in epoch milliseconds.
    pub ttl: Option<i64>,
    /// Length of the stored bytes (0 for tombstones).
    pub len: usize,
    /// True if this entry records a delete.
    pub tombstone: bool,
    /// True if this version hasn't been written to an SSTable yet.
    pub dirty: bool,
    last_access: u64,
}

impl EntryMeta {
    pub fn is_expired(&self, now_millis: i64) -> bool {
        self.ttl.is_some_and(|ttl| ttl <= now_millis)
    }
}

/// Result of reading an entry.
#[derive(Debug, PartialEq)]
pub enum Lookup {
    Live { bytes: Vec<u8>, seq: u64, ttl: Option<i64> },
    Tombstone { seq: u64 },
    /// Too many shards are corrupt to rebuild the document.
    Unrecoverable { seq: u64 },
}

/// A dirty entry captured for flushing.
#[derive(Debug)]
pub struct DirtyEntry {
    pub key: DocKey,
    pub seq: u64,
    pub ttl: Option<i64>,
    /// `None` for tombstones.
    pub bytes: Option<Vec<u8>>,
}

/// Outcome of an integrity check.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct IntegrityReport {
    pub checked: usize,
    pub corrupt_shards: usize,
    pub repaired_shards: usize,
    pub unrecoverable: usize,
}

pub struct Hex {
    pub vertices: [Vertex; VERTEX_COUNT],
    /// Ordered by tessellation, then ID, so one tessellation's entries are a range.
    entries: BTreeMap<DocKey, EntryMeta>,
    clock: u64,
    dirty_bytes: usize,
    dirty_count: usize,
}

/// The range of keys belonging to one tessellation, optionally after an ID.
fn tessellation_range(tessellation: &str, after: Option<Ulid>) -> (Bound<DocKey>, Bound<DocKey>) {
    let start = match after {
        Some(id) => Bound::Excluded(DocKey::new(tessellation, id)),
        None => Bound::Included(DocKey::new(tessellation, Ulid::nil())),
    };
    (start, Bound::Included(DocKey::new(tessellation, Ulid::from(u128::MAX))))
}

/// Split bytes into four data shards (zero-padded to an even length, as the
/// codec requires) and compute two parity shards: six shards in all.
fn encode(bytes: &[u8]) -> Vec<Vec<u8>> {
    let shard_len = bytes.len().div_ceil(DATA_SHARDS).max(1).next_multiple_of(2);
    let mut shards: Vec<Vec<u8>> = (0..DATA_SHARDS)
        .map(|i| {
            let start = (i * shard_len).min(bytes.len());
            let end = (start + shard_len).min(bytes.len());
            let mut shard = bytes[start..end].to_vec();
            shard.resize(shard_len, 0);
            shard
        })
        .collect();
    if shard_len < LARGE_SHARD {
        shards.extend(crate::erasure::encode(&shards));
    } else {
        let parity = reed_solomon_simd::encode(DATA_SHARDS, PARITY_SHARDS, &shards).expect("shards have equal, even length");
        shards.extend(parity);
    }
    shards
}

/// Shards this long or longer use reed-solomon-simd (fastest on large
/// shards); shorter ones use the table-driven code in `erasure` (no per-call
/// setup). Both encoding and repair choose by shard length.
const LARGE_SHARD: usize = 16 * 1024;

/// Fill in missing shards (`None`) from any four present ones.
fn reconstruct(shards: &mut [Option<Vec<u8>>]) -> Result<(), ()> {
    let present = shards.iter().filter(|s| s.is_some()).count();
    if present < DATA_SHARDS {
        return Err(());
    }
    if present == VERTEX_COUNT {
        return Ok(());
    }
    let len = shards.iter().flatten().next().map(Vec::len).unwrap_or(0);
    if len < LARGE_SHARD {
        return crate::erasure::reconstruct(shards);
    }
    let originals: Vec<(usize, &Vec<u8>)> = shards[..DATA_SHARDS].iter().enumerate().filter_map(|(i, s)| s.as_ref().map(|s| (i, s))).collect();
    let recovery: Vec<(usize, &Vec<u8>)> = shards[DATA_SHARDS..].iter().enumerate().filter_map(|(i, s)| s.as_ref().map(|s| (i, s))).collect();
    let restored = if originals.len() == DATA_SHARDS {
        Default::default()
    } else {
        reed_solomon_simd::decode(DATA_SHARDS, PARITY_SHARDS, originals, recovery).map_err(|_| ())?
    };
    for (i, shard) in restored {
        shards[i] = Some(shard);
    }
    // With every data shard back, recompute missing parity.
    if shards[DATA_SHARDS..].iter().any(Option::is_none) {
        let data: Vec<&Vec<u8>> = shards[..DATA_SHARDS].iter().map(|s| s.as_ref().unwrap()).collect();
        let parity = reed_solomon_simd::encode(DATA_SHARDS, PARITY_SHARDS, data).map_err(|_| ())?;
        for (i, p) in parity.into_iter().enumerate() {
            if shards[DATA_SHARDS + i].is_none() {
                shards[DATA_SHARDS + i] = Some(p);
            }
        }
    }
    Ok(())
}

impl Default for Hex {
    fn default() -> Self {
        Self::new()
    }
}

impl Hex {
    /// Creates a new, empty hex with six vertices.
    pub fn new() -> Self {
        Hex {
            vertices: std::array::from_fn(Vertex::new),
            entries: BTreeMap::new(),
            clock: 0,
            dirty_bytes: 0,
            dirty_count: 0,
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    // -----------------------------------------------------------------------
    // Writes
    // -----------------------------------------------------------------------

    /// Store a document version. Replaces any existing entry for the key.
    pub fn put(&mut self, key: &DocKey, seq: u64, ttl: Option<i64>, bytes: &[u8], dirty: bool) {
        self.remove(key);

        let shards = self.encode(bytes);
        for (vertex, shard) in self.vertices.iter_mut().zip(shards) {
            vertex.store(key, shard);
        }

        if dirty {
            self.dirty_bytes += bytes.len();
            self.dirty_count += 1;
        }
        let last_access = self.tick();
        self.entries.insert(
            key.clone(),
            EntryMeta { seq, ttl, len: bytes.len(), tombstone: false, dirty, last_access },
        );
    }

    /// Record a delete. Replaces any existing entry for the key.
    pub fn put_tombstone(&mut self, key: &DocKey, seq: u64, dirty: bool) {
        self.remove(key);
        if dirty {
            self.dirty_count += 1;
        }
        let last_access = self.tick();
        self.entries.insert(
            key.clone(),
            EntryMeta { seq, ttl: None, len: 0, tombstone: true, dirty, last_access },
        );
    }

    /// Drop an entry entirely (eviction, or tessellation drop).
    pub fn remove(&mut self, key: &DocKey) -> Option<EntryMeta> {
        let meta = self.entries.remove(key)?;
        if meta.dirty {
            self.dirty_bytes = self.dirty_bytes.saturating_sub(meta.len);
            self.dirty_count = self.dirty_count.saturating_sub(1);
        }
        if !meta.tombstone {
            for vertex in self.vertices.iter_mut() {
                vertex.remove(key);
            }
        }
        Some(meta)
    }

    /// Drop every entry belonging to a tessellation.
    pub fn remove_tessellation(&mut self, tessellation: &str) -> usize {
        let keys: Vec<DocKey> = self.entries.range(tessellation_range(tessellation, None)).map(|(k, _)| k.clone()).collect();
        for key in &keys {
            self.remove(key);
        }
        keys.len()
    }

    // -----------------------------------------------------------------------
    // Reads
    // -----------------------------------------------------------------------

    pub fn meta(&self, key: &DocKey) -> Option<&EntryMeta> {
        self.entries.get(key)
    }

    /// Read an entry, rebuilding it from parity if needed.
    pub fn read(&mut self, key: &DocKey) -> Option<Lookup> {
        let now = self.tick();
        let meta = self.entries.get_mut(key)?;
        meta.last_access = now;
        let meta = meta.clone();

        if meta.tombstone {
            return Some(Lookup::Tombstone { seq: meta.seq });
        }
        Some(match self.decode(key, meta.len) {
            Some(bytes) => Lookup::Live { bytes, seq: meta.seq, ttl: meta.ttl },
            None => Lookup::Unrecoverable { seq: meta.seq },
        })
    }

    /// Entries (key and metadata) for one tessellation, in ID order.
    pub fn entries_in(&self, tessellation: &str) -> Vec<(DocKey, EntryMeta)> {
        self.entries
            .range(tessellation_range(tessellation, None))
            .map(|(k, m)| (k.clone(), m.clone()))
            .collect()
    }

    /// Up to `limit` IDs of a tessellation after `after`, in ID order, with metadata.
    pub fn ids_after(&self, tessellation: &str, after: Option<Ulid>, limit: usize) -> Vec<(Ulid, EntryMeta)> {
        self.entries
            .range(tessellation_range(tessellation, after))
            .take(limit)
            .map(|(k, m)| (k.id, m.clone()))
            .collect()
    }

    pub fn keys(&self) -> Vec<DocKey> {
        self.entries.keys().cloned().collect()
    }

    // -----------------------------------------------------------------------
    // Flushing and eviction
    // -----------------------------------------------------------------------

    /// Capture every dirty entry for flushing. Entries stay dirty until
    /// [`Hex::mark_clean`] is called after the flush succeeds.
    pub fn dirty_snapshot(&mut self) -> Result<Vec<DirtyEntry>, DocKey> {
        let dirty: Vec<(DocKey, EntryMeta)> = self
            .entries
            .iter()
            .filter(|(_, m)| m.dirty)
            .map(|(k, m)| (k.clone(), m.clone()))
            .collect();

        let mut out = Vec::with_capacity(dirty.len());
        for (key, meta) in dirty {
            let bytes = if meta.tombstone {
                None
            } else {
                Some(self.decode(&key, meta.len).ok_or_else(|| key.clone())?)
            };
            out.push(DirtyEntry { key, seq: meta.seq, ttl: meta.ttl, bytes });
        }
        Ok(out)
    }

    /// Mark flushed entries clean, unless they were overwritten since the snapshot.
    pub fn mark_clean(&mut self, flushed: &[(DocKey, u64)]) {
        for (key, seq) in flushed {
            if let Some(meta) = self.entries.get_mut(key) {
                if meta.seq == *seq && meta.dirty {
                    meta.dirty = false;
                    self.dirty_bytes = self.dirty_bytes.saturating_sub(meta.len);
                    self.dirty_count = self.dirty_count.saturating_sub(1);
                }
            }
        }
    }

    /// Evict clean entries, least recently used first, until shard memory is at
    /// or below `target_bytes`. Clean tombstones are always evicted. Returns the
    /// number of entries evicted.
    pub fn evict_clean(&mut self, target_bytes: usize) -> usize {
        let mut candidates: Vec<(u64, DocKey, bool)> = self
            .entries
            .iter()
            .filter(|(_, m)| !m.dirty)
            .map(|(k, m)| (m.last_access, k.clone(), m.tombstone))
            .collect();
        candidates.sort_by_key(|(access, _, _)| *access);

        let mut evicted = 0;
        for (_, key, tombstone) in candidates {
            if !tombstone && self.memory_bytes() <= target_bytes {
                continue;
            }
            self.remove(&key);
            evicted += 1;
        }
        evicted
    }

    /// Evict clean entries whose TTL has passed. Dirty ones stay until flushed,
    /// so an older version on disk can't reappear.
    pub fn evict_expired_clean(&mut self, now_millis: i64) -> usize {
        let expired: Vec<DocKey> = self
            .entries
            .iter()
            .filter(|(_, m)| !m.dirty && m.is_expired(now_millis))
            .map(|(k, _)| k.clone())
            .collect();
        for key in &expired {
            self.remove(key);
        }
        expired.len()
    }

    // -----------------------------------------------------------------------
    // Integrity
    // -----------------------------------------------------------------------

    /// Verify the shards of the given keys and rebuild any corrupt ones.
    pub fn check_integrity(&mut self, keys: &[DocKey]) -> IntegrityReport {
        let mut report = IntegrityReport::default();

        for key in keys {
            let Some(meta) = self.entries.get(key) else { continue };
            if meta.tombstone {
                continue;
            }
            report.checked += 1;

            let mut shards: Vec<Option<Vec<u8>>> =
                self.vertices.iter().map(|v| v.intact_copy(key)).collect();
            let corrupt: Vec<usize> = (0..VERTEX_COUNT).filter(|&i| shards[i].is_none()).collect();
            if corrupt.is_empty() {
                continue;
            }

            report.corrupt_shards += corrupt.len();
            for &i in &corrupt {
                self.vertices[i].corrupt_found += 1;
            }

            if reconstruct(&mut shards).is_err() {
                report.unrecoverable += 1;
                continue;
            }
            for i in corrupt {
                if let Some(bytes) = shards[i].take() {
                    self.vertices[i].store(key, bytes);
                    self.vertices[i].repaired += 1;
                    report.repaired_shards += 1;
                }
            }
        }

        report
    }

    // -----------------------------------------------------------------------
    // Statistics
    // -----------------------------------------------------------------------

    /// Number of entries (live and tombstones).
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Number of live (non-tombstone) entries.
    pub fn live_count(&self) -> usize {
        self.entries.values().filter(|m| !m.tombstone).count()
    }

    pub fn dirty_count(&self) -> usize {
        self.dirty_count
    }

    /// Bytes of unflushed document data.
    pub fn dirty_bytes(&self) -> usize {
        self.dirty_bytes
    }

    /// Total shard bytes held across all vertices.
    pub fn memory_bytes(&self) -> usize {
        self.vertices.iter().map(Vertex::bytes).sum()
    }

    /// Original (un-sharded) byte lengths of live entries, per tessellation.
    pub fn live_sizes(&self) -> HashMap<String, Vec<usize>> {
        let mut sizes: HashMap<String, Vec<usize>> = HashMap::new();
        for (key, meta) in &self.entries {
            if !meta.tombstone {
                sizes.entry(key.tessellation.clone()).or_default().push(meta.len);
            }
        }
        sizes
    }

    // -----------------------------------------------------------------------
    // Encoding
    // -----------------------------------------------------------------------

    fn encode(&self, bytes: &[u8]) -> Vec<Vec<u8>> {
        encode(bytes)
    }

    fn decode(&self, key: &DocKey, len: usize) -> Option<Vec<u8>> {
        // Fast path: all data shards intact.
        let mut out = Vec::with_capacity(len + 2 * DATA_SHARDS);
        let mut intact = true;
        for vertex in &self.vertices[..DATA_SHARDS] {
            match vertex.get(key) {
                Some(shard) if shard.is_intact() => out.extend_from_slice(&shard.bytes),
                _ => {
                    intact = false;
                    break;
                }
            }
        }
        if intact {
            out.truncate(len);
            return Some(out);
        }

        // Rebuild from any four intact shards.
        let mut shards: Vec<Option<Vec<u8>>> = self.vertices.iter().map(|v| v.intact_copy(key)).collect();
        reconstruct(&mut shards).ok()?;
        let mut out: Vec<u8> = shards.into_iter().take(DATA_SHARDS).flat_map(|s| s.unwrap_or_default()).collect();
        out.truncate(len);
        Some(out)
    }

    /// Corrupt one vertex's shard of a document. Testing aid only.
    #[doc(hidden)]
    pub fn corrupt_for_testing(&mut self, key: &DocKey, vertex: usize) -> bool {
        self.vertices[vertex].corrupt_for_testing(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u128) -> DocKey {
        DocKey::new("t", Ulid::from(n))
    }

    #[test]
    fn stores_and_reads_back() {
        let mut hex = Hex::new();
        for len in [0usize, 1, 3, 4, 5, 100, 1001] {
            let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
            hex.put(&key(len as u128), 1, None, &data, true);
            assert_eq!(
                hex.read(&key(len as u128)),
                Some(Lookup::Live { bytes: data, seq: 1, ttl: None })
            );
        }
    }

    #[test]
    fn survives_two_corrupt_vertices_and_repairs() {
        let mut hex = Hex::new();
        let data = b"the quick brown fox jumps over the lazy dog".to_vec();
        let k = key(1);
        hex.put(&k, 7, None, &data, true);

        assert!(hex.corrupt_for_testing(&k, 0));
        assert!(hex.corrupt_for_testing(&k, 3));
        assert_eq!(hex.read(&k), Some(Lookup::Live { bytes: data.clone(), seq: 7, ttl: None }));

        let report = hex.check_integrity(std::slice::from_ref(&k));
        assert_eq!(report.corrupt_shards, 2);
        assert_eq!(report.repaired_shards, 2);
        assert_eq!(hex.check_integrity(std::slice::from_ref(&k)).corrupt_shards, 0);
        assert_eq!(hex.read(&k), Some(Lookup::Live { bytes: data, seq: 7, ttl: None }));
    }

    #[test]
    fn large_documents_use_the_simd_code_and_repair_too() {
        let mut hex = Hex::new();
        let data: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let k = key(2);
        hex.put(&k, 3, None, &data, true);
        for pair in [(0, 1), (2, 5), (4, 5)] {
            assert!(hex.corrupt_for_testing(&k, pair.0));
            assert!(hex.corrupt_for_testing(&k, pair.1));
            assert_eq!(hex.read(&k), Some(Lookup::Live { bytes: data.clone(), seq: 3, ttl: None }));
            assert_eq!(hex.check_integrity(std::slice::from_ref(&k)).repaired_shards, 2);
        }
    }

    #[test]
    fn three_corrupt_vertices_are_unrecoverable() {
        let mut hex = Hex::new();
        let k = key(1);
        hex.put(&k, 1, None, b"some document bytes", true);
        for v in [0, 1, 5] {
            hex.corrupt_for_testing(&k, v);
        }
        assert_eq!(hex.read(&k), Some(Lookup::Unrecoverable { seq: 1 }));
        assert_eq!(hex.check_integrity(&[k]).unrecoverable, 1);
    }

    #[test]
    fn tombstones_and_dirty_tracking() {
        let mut hex = Hex::new();
        let k = key(1);
        hex.put(&k, 1, None, b"abcdef", true);
        assert_eq!(hex.dirty_bytes(), 6);

        hex.put_tombstone(&k, 2, true);
        assert_eq!(hex.read(&k), Some(Lookup::Tombstone { seq: 2 }));
        assert_eq!(hex.dirty_bytes(), 0);
        assert_eq!(hex.memory_bytes(), 0);

        let snapshot = hex.dirty_snapshot().unwrap();
        assert_eq!(snapshot.len(), 1);
        assert!(snapshot[0].bytes.is_none());
    }

    #[test]
    fn mark_clean_skips_overwritten_entries() {
        let mut hex = Hex::new();
        let k = key(1);
        hex.put(&k, 1, None, b"v1", true);
        let snapshot: Vec<(DocKey, u64)> =
            hex.dirty_snapshot().unwrap().into_iter().map(|e| (e.key, e.seq)).collect();
        hex.put(&k, 2, None, b"v2", true); // overwritten during the flush
        hex.mark_clean(&snapshot);
        assert!(hex.meta(&k).unwrap().dirty);
    }

    #[test]
    fn eviction_keeps_dirty_and_recent_entries() {
        let mut hex = Hex::new();
        let data = vec![1u8; 400];
        for n in 0..10 {
            hex.put(&key(n), n as u64, None, &data, false);
        }
        hex.put(&key(100), 100, None, &data, true); // dirty
        hex.read(&key(9)); // most recently used

        let per_entry = hex.memory_bytes() / 11;
        let evicted = hex.evict_clean(per_entry * 3);
        assert_eq!(evicted, 8);
        assert!(hex.meta(&key(100)).is_some(), "dirty entries are never evicted");
        assert!(hex.meta(&key(9)).is_some(), "most recently used survives");
        assert!(hex.memory_bytes() <= per_entry * 3);
    }

    #[test]
    fn expired_clean_entries_are_evicted() {
        let mut hex = Hex::new();
        hex.put(&key(1), 1, Some(100), b"old", false);
        hex.put(&key(2), 2, Some(100), b"dirty", true);
        hex.put(&key(3), 3, Some(10_000), b"fresh", false);
        assert_eq!(hex.evict_expired_clean(500), 1);
        assert!(hex.meta(&key(1)).is_none());
        assert!(hex.meta(&key(2)).is_some());
        assert!(hex.meta(&key(3)).is_some());
    }
}
