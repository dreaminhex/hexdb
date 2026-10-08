// HexDB Core Engine: read paths
//
// Listing pages through a tessellation in ID order by merging the memory
// store and each SSTable's sorted index from the cursor, so a page costs about
// its own size instead of a scan of every key.
//
// Document counts are cached per tessellation and kept exact by commits: each
// write adds or removes one depending on whether the document was visible
// before it, so a busy tessellation isn't rescanned for every page's total.
// A count is recomputed only when first needed, after its tessellation is
// dropped, or once the earliest TTL among the counted documents passes.
//
// Each tessellation also has a write generation. Other statistics (sizes for
// `/status`) are cached until their tessellation's generation moves, so writes
// to one tessellation don't invalidate another's figures. A count computed
// while a commit was running is stored only if the generation didn't move.
//
// Parsed documents are cached by (key, version), so repeated reads of the same
// version skip shard decoding and JSON parsing. The cache holds at most a
// quarter of the RAM budget, measured by stored size.

use super::{HexDBEngine, TessellationStats};
use crate::{document::Document, hex::DocKey};
use chrono::Utc;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use ulid::Ulid;

/// Parsed documents by key and version.
pub(crate) struct DocCache {
    inner: std::sync::Mutex<DocCacheInner>,
}

struct DocCacheInner {
    map: HashMap<DocKey, (u64, Document, usize, u64)>,
    bytes: usize,
    budget: usize,
    tick: u64,
}

impl DocCache {
    pub(crate) fn new(budget: usize) -> Self {
        DocCache { inner: std::sync::Mutex::new(DocCacheInner { map: HashMap::new(), bytes: 0, budget: budget.max(1 << 20), tick: 0 }) }
    }

    pub(crate) fn get(&self, key: &DocKey, seq: u64) -> Option<Document> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.tick += 1;
        let tick = inner.tick;
        let (cached_seq, doc, _, last) = inner.map.get_mut(key)?;
        if *cached_seq != seq {
            return None;
        }
        *last = tick;
        Some(doc.clone())
    }

    pub(crate) fn put(&self, key: &DocKey, seq: u64, doc: &Document, size: usize) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if size > inner.budget / 8 {
            return;
        }
        inner.tick += 1;
        let tick = inner.tick;
        if let Some((_, _, old, _)) = inner.map.insert(key.clone(), (seq, doc.clone(), size, tick)) {
            inner.bytes -= old;
        }
        inner.bytes += size;
        if inner.bytes > inner.budget {
            // Drop the least recently used quarter.
            let mut by_age: Vec<(u64, DocKey)> = inner.map.iter().map(|(k, (_, _, _, t))| (*t, k.clone())).collect();
            by_age.sort_unstable_by_key(|(t, _)| *t);
            let target = inner.budget * 3 / 4;
            for (_, key) in by_age {
                if inner.bytes <= target {
                    break;
                }
                if let Some((_, _, size, _)) = inner.map.remove(&key) {
                    inner.bytes -= size;
                }
            }
        }
    }

    pub(crate) fn drop_tessellation(&self, tess: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut freed = 0;
        inner.map.retain(|k, (_, _, size, _)| {
            let keep = k.tessellation != tess;
            if !keep {
                freed += *size;
            }
            keep
        });
        inner.bytes -= freed;
    }
}

/// Write generations and cached document counts, per tessellation.
#[derive(Default)]
pub(crate) struct Counts {
    generations: HashMap<String, u64>,
    cached: HashMap<String, CachedCount>,
}

struct CachedCount {
    count: usize,
    /// Epoch ms when a counted document expires (the count is stale after it).
    valid_until: i64,
}

/// How a commit changed one tessellation's count: the difference, and the
/// earliest TTL among the documents it made visible.
#[derive(Default, Clone, Copy)]
pub(crate) struct CountDelta {
    pub(crate) delta: i64,
    pub(crate) earliest_ttl: Option<i64>,
}

/// Cached statistics for one tessellation.
#[derive(Clone)]
pub(crate) struct CachedStats {
    generation: u64,
    /// Epoch ms when a counted document expires (the cache is stale after it).
    valid_until: i64,
    stats: TessellationStats,
}

impl HexDBEngine {
    /// Note that data changed: cached counts and statistics are stale.
    /// The current write generation (changes whenever stored data changes).
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// A tessellation's write generation.
    fn tess_generation(&self, tess: &str) -> u64 {
        self.counts.lock().unwrap().generations.get(tess).copied().unwrap_or(0)
    }

    /// Tessellations whose count is cached (a commit computes deltas for these).
    pub(crate) fn counted_tessellations(&self) -> std::collections::HashSet<String> {
        self.counts.lock().unwrap().cached.keys().cloned().collect()
    }

    /// Record that these tessellations changed: bump their generations and
    /// apply count deltas. A cached count without a delta is dropped (it may
    /// have been stored after the commit decided which counts to adjust).
    pub(crate) fn note_writes(&self, touched: &[String], deltas: &HashMap<String, CountDelta>) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let mut counts = self.counts.lock().unwrap();
        for tess in touched {
            *counts.generations.entry(tess.clone()).or_default() += 1;
            match (counts.cached.get_mut(tess), deltas.get(tess)) {
                (Some(cached), Some(d)) => {
                    cached.count = (cached.count as i64 + d.delta).max(0) as usize;
                    if let Some(t) = d.earliest_ttl {
                        cached.valid_until = cached.valid_until.min(t);
                    }
                }
                (Some(_), None) => {
                    counts.cached.remove(tess);
                }
                (None, _) => {}
            }
        }
    }

    /// Visible documents in a tessellation: the cached count, or a fresh one.
    pub(crate) async fn cached_count(&self, tess: &str) -> usize {
        let now = Utc::now().timestamp_millis();
        let generation = {
            let counts = self.counts.lock().unwrap();
            if let Some(c) = counts.cached.get(tess) {
                if now < c.valid_until {
                    return c.count;
                }
            }
            counts.generations.get(tess).copied().unwrap_or(0)
        };
        let stats = self.cached_stats(tess).await;
        let valid_until = self.stats_cache.lock().unwrap().get(tess).map(|c| c.valid_until).unwrap_or(i64::MIN);
        let mut counts = self.counts.lock().unwrap();
        if counts.generations.get(tess).copied().unwrap_or(0) == generation && now < valid_until {
            counts.cached.insert(tess.to_string(), CachedCount { count: stats.document_count, valid_until });
        }
        stats.document_count
    }

    /// Statistics for a tessellation, from the cache when it's still valid.
    pub(crate) async fn cached_stats(&self, tess: &str) -> TessellationStats {
        let now = Utc::now().timestamp_millis();
        let generation = self.tess_generation(tess);
        if let Some(cached) = self.stats_cache.lock().unwrap().get(tess) {
            if cached.generation == generation && now < cached.valid_until {
                return cached.stats.clone();
            }
        }
        let (stats, valid_until) = self.compute_stats(tess, now).await;
        if self.tess_generation(tess) == generation {
            self.stats_cache
                .lock()
                .unwrap()
                .insert(tess.to_string(), CachedStats { generation, valid_until, stats: stats.clone() });
        }
        stats
    }

    /// IDs of visible documents in a tessellation after `after`, in ID order,
    /// at most `limit`. Reads only about as many keys as it returns.
    pub(crate) async fn visible_ids_after(&self, tess: &str, after: Option<Ulid>, limit: usize) -> Vec<Ulid> {
        const BATCH: usize = 256;
        let mut out = Vec::new();
        let mut cursor = after;
        while out.len() < limit {
            let now = Utc::now().timestamp_millis();
            let memory = self.state.lock().await.hex.ids_after(tess, cursor, BATCH);
            let disk = self.sst.entries_after(tess, cursor, BATCH);
            let disk = disk.await;
            if memory.is_empty() && disk.is_empty() {
                break;
            }
            // IDs up to the smaller "last ID" of a full batch are complete in both lists.
            let bound = |len: usize, last: Option<Ulid>| if len == BATCH { last } else { None };
            let complete_to = match (bound(memory.len(), memory.last().map(|(id, _)| *id)), bound(disk.len(), disk.last().map(|(id, _)| *id))) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let mut merged: std::collections::BTreeMap<Ulid, (u64, bool)> = std::collections::BTreeMap::new();
            for (id, entry) in disk {
                merged.insert(id, (entry.seq, !entry.tombstone && !entry.is_expired(now)));
            }
            for (id, meta) in memory {
                if merged.get(&id).is_none_or(|(seq, _)| *seq <= meta.seq) {
                    merged.insert(id, (meta.seq, !meta.tombstone && !meta.is_expired(now)));
                }
            }
            for (id, (_, visible)) in merged {
                if complete_to.is_some_and(|c| id > c) {
                    break;
                }
                if visible {
                    out.push(id);
                    if out.len() == limit {
                        break;
                    }
                }
            }
            match complete_to {
                None => break,
                Some(c) => cursor = Some(c),
            }
        }
        out
    }
}
