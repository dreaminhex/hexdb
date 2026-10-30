// HexDB Core Vectors
//
// The store behind vector indexes (see `crate::index`): one vector per
// document, compared by cosine similarity (vectors are normalized when
// stored, so it's a dot product) or by plain dot product.
//
// Up to `EXACT_LIMIT` vectors, a search scores every one (exact, and fast at
// that size). Beyond it, an HNSW graph (Malkov and Yashunin, "Efficient and
// robust approximate nearest neighbor search using Hierarchical Navigable
// Small World graphs") finds the nearest ones: each vector is linked to its
// nearest neighbors on layer 0 and, with exponentially falling probability,
// on higher layers; a search descends greedily from the top layer's entry
// point and then explores layer 0 with a candidate list of `ef` entries.
// Removed vectors stay in the graph as waypoints but are never returned; the
// graph is rebuilt once they outnumber the live ones.
//
// With int8 quantization a vector is kept as one signed byte per dimension
// and a scale (its largest magnitude / 127), a quarter of the memory; scores
// are computed against the dequantized values, so rankings are approximate
// (searches rescore their results exactly; see `engine::vectors`).

use rand::RngExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use ulid::Ulid;

/// Most dimensions a vector may have.
pub const MAX_DIMENSIONS: usize = 4096;
/// Up to this many vectors, searches score every vector.
pub const EXACT_LIMIT: usize = 2000;
/// Links per vector on layers above 0 (twice that on layer 0).
const M: usize = 16;
/// Candidate list size while inserting.
const EF_CONSTRUCTION: usize = 100;
const MAX_LEVEL: usize = 16;

/// How vectors are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Metric {
    /// Cosine similarity, -1 to 1 (vectors are normalized).
    #[default]
    Cosine,
    /// Dot product, for vectors whose length means something.
    Dot,
}

/// A vector held in a document field: an array of numbers, or an object with
/// one under `vector` (as auto-embedding writes it).
pub fn extract(value: &Value) -> Option<Vec<f32>> {
    let items = match value {
        Value::Array(items) => items,
        Value::Object(map) => map.get("vector")?.as_array()?,
        _ => return None,
    };
    if items.is_empty() || items.len() > MAX_DIMENSIONS {
        return None;
    }
    items.iter().map(|v| v.as_f64().map(|f| f as f32).filter(|f| f.is_finite())).collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// The vector scaled to length 1 (None for a zero vector).
pub fn normalized(mut v: Vec<f32>) -> Option<Vec<f32>> {
    let norm = dot(&v, &v).sqrt();
    if norm == 0.0 || !norm.is_finite() {
        return None;
    }
    v.iter_mut().for_each(|x| *x /= norm);
    Some(v)
}

/// How vectors are kept in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Quantization {
    /// 4 bytes per dimension, exact.
    #[default]
    None,
    /// 1 byte per dimension plus a scale per vector.
    Int8,
}

/// The stored vectors, one per slot.
enum Vectors {
    F32(Vec<Vec<f32>>),
    /// (codes, scale): value = code * scale.
    I8(Vec<(Vec<i8>, f32)>),
}

impl Vectors {
    fn len(&self) -> usize {
        match self {
            Vectors::F32(v) => v.len(),
            Vectors::I8(v) => v.len(),
        }
    }

    fn push(&mut self, v: Vec<f32>) {
        match self {
            Vectors::F32(all) => all.push(v),
            Vectors::I8(all) => all.push(quantize(&v)),
        }
    }

    /// A slot's vector as numbers (dequantized).
    fn get(&self, slot: u32) -> Vec<f32> {
        match self {
            Vectors::F32(all) => all[slot as usize].clone(),
            Vectors::I8(all) => {
                let (codes, scale) = &all[slot as usize];
                codes.iter().map(|&c| c as f32 * scale).collect()
            }
        }
    }

    /// The dot product of a query with a slot's vector.
    fn dot(&self, query: &[f32], slot: u32) -> f32 {
        match self {
            Vectors::F32(all) => dot(query, &all[slot as usize]),
            Vectors::I8(all) => {
                let (codes, scale) = &all[slot as usize];
                query.iter().zip(codes).map(|(q, &c)| q * c as f32).sum::<f32>() * scale
            }
        }
    }
}

/// One byte per value, scaled by the largest magnitude.
pub fn quantize(v: &[f32]) -> (Vec<i8>, f32) {
    let max = v.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    if max == 0.0 {
        return (vec![0; v.len()], 0.0);
    }
    let scale = max / 127.0;
    (v.iter().map(|x| (x / scale).round().clamp(-127.0, 127.0) as i8).collect(), scale)
}

/// A score ordered by `total_cmp`, for heaps.
#[derive(Clone, Copy, PartialEq)]
struct Scored(f32, u32);

impl Eq for Scored {}

impl PartialOrd for Scored {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Scored {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0).then(self.1.cmp(&other.1))
    }
}

/// Vectors by document, with their HNSW graph.
pub struct VectorStore {
    pub metric: Metric,
    /// Set by the index definition, or by the first vector stored.
    pub dimensions: Option<usize>,
    pub quantization: Quantization,
    ids: Vec<Ulid>,
    vectors: Vectors,
    deleted: Vec<bool>,
    /// links[slot][layer]
    links: Vec<Vec<Vec<u32>>>,
    slot: HashMap<Ulid, u32>,
    entry: Option<u32>,
    top: usize,
    removed: usize,
}

impl VectorStore {
    pub fn new(metric: Metric, dimensions: Option<usize>) -> Self {
        Self::with_quantization(metric, dimensions, Quantization::None)
    }

    pub fn with_quantization(metric: Metric, dimensions: Option<usize>, quantization: Quantization) -> Self {
        let vectors = match quantization {
            Quantization::None => Vectors::F32(Vec::new()),
            Quantization::Int8 => Vectors::I8(Vec::new()),
        };
        VectorStore { metric, dimensions, quantization, ids: Vec::new(), vectors, deleted: Vec::new(), links: Vec::new(), slot: HashMap::new(), entry: None, top: 0, removed: 0 }
    }

    /// Bytes the vectors take (not counting the graph).
    pub fn vector_bytes(&self) -> usize {
        let dims = self.dimensions.unwrap_or(0);
        let per = match self.quantization {
            Quantization::None => dims * 4,
            Quantization::Int8 => dims + 4,
        };
        self.slot.len() * per
    }

    /// The exact score of a prepared query against a vector (for rescoring
    /// quantized results). None if the vector doesn't fit.
    pub fn exact_score(&self, query: &[f32], vector: Vec<f32>) -> Option<f32> {
        self.prepare(vector).map(|v| dot(query, &v))
    }

    /// Live vectors.
    pub fn len(&self) -> usize {
        self.slot.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slot.is_empty()
    }

    pub fn contains(&self, id: &Ulid) -> bool {
        self.slot.contains_key(id)
    }

    /// Every live vector (normalized for cosine), for snapshots.
    pub fn entries(&self) -> Vec<(Ulid, Vec<f32>)> {
        self.slot.iter().map(|(id, s)| (*id, self.vectors.get(*s))).collect()
    }

    /// The vector a query or document compares with: normalized for cosine,
    /// None if it has the wrong number of dimensions or no length.
    pub fn prepare(&self, v: Vec<f32>) -> Option<Vec<f32>> {
        if self.dimensions.is_some_and(|d| d != v.len()) {
            return None;
        }
        match self.metric {
            Metric::Cosine => normalized(v),
            Metric::Dot => Some(v),
        }
    }

    /// Store (or replace) a document's vector. False if it doesn't fit.
    pub fn insert(&mut self, id: Ulid, v: Vec<f32>) -> bool {
        self.remove(&id);
        let Some(v) = self.prepare(v) else { return false };
        self.dimensions.get_or_insert(v.len());
        self.add(id, v);
        true
    }

    fn add(&mut self, id: Ulid, v: Vec<f32>) {
        let slot = self.vectors.len() as u32;
        let level = random_level();
        self.ids.push(id);
        self.vectors.push(v);
        self.deleted.push(false);
        self.links.push(vec![Vec::new(); level + 1]);
        self.slot.insert(id, slot);
        let Some(mut entry) = self.entry else {
            self.entry = Some(slot);
            self.top = level;
            return;
        };
        let query = self.vectors.get(slot);
        for layer in (level + 1..=self.top).rev() {
            entry = self.greedy(&query, entry, layer);
        }
        for layer in (0..=level.min(self.top)).rev() {
            let found = self.search_layer(&query, &[entry], EF_CONSTRUCTION, layer);
            let width = if layer == 0 { 2 * M } else { M };
            let chosen: Vec<u32> = found.iter().filter(|s| s.1 != slot).take(width).map(|s| s.1).collect();
            for &other in &chosen {
                let list = &mut self.links[other as usize][layer];
                list.push(slot);
                if list.len() > width {
                    self.prune(other, layer, width);
                }
            }
            self.links[slot as usize][layer] = chosen;
            if let Some(best) = found.first() {
                entry = best.1;
            }
        }
        if level > self.top {
            self.top = level;
            self.entry = Some(slot);
        }
    }

    /// Keep a node's `width` nearest links on a layer.
    fn prune(&mut self, node: u32, layer: usize, width: usize) {
        let base = self.vectors.get(node);
        let mut scored: Vec<Scored> = self.links[node as usize][layer].iter().map(|&n| Scored(self.vectors.dot(&base, n), n)).collect();
        scored.sort_by(|a, b| b.cmp(a));
        self.links[node as usize][layer] = scored.into_iter().take(width).map(|s| s.1).collect();
    }

    /// Forget a document's vector.
    pub fn remove(&mut self, id: &Ulid) {
        let Some(slot) = self.slot.remove(id) else { return };
        self.deleted[slot as usize] = true;
        // Kept as a waypoint in the graph; its links still need its vector.
        self.removed += 1;
        if self.removed > 1000 && self.removed > self.slot.len() {
            self.rebuild();
        }
    }

    /// A new graph of the live vectors.
    fn rebuild(&mut self) {
        let live: Vec<(Ulid, Vec<f32>)> = self.slot.iter().map(|(id, s)| (*id, self.vectors.get(*s))).collect();
        *self = VectorStore::with_quantization(self.metric, self.dimensions, self.quantization);
        for (id, v) in live {
            self.add(id, v);
        }
    }

    fn score(&self, query: &[f32], slot: u32) -> f32 {
        self.vectors.dot(query, slot)
    }

    fn greedy(&self, query: &[f32], mut current: u32, layer: usize) -> u32 {
        let mut best = self.score(query, current);
        loop {
            let mut moved = false;
            for &n in self.links[current as usize].get(layer).into_iter().flatten() {
                let s = self.score(query, n);
                if s > best {
                    best = s;
                    current = n;
                    moved = true;
                }
            }
            if !moved {
                return current;
            }
        }
    }

    /// The `ef` best nodes reachable on a layer, best first (removed ones included).
    fn search_layer(&self, query: &[f32], entries: &[u32], ef: usize, layer: usize) -> Vec<Scored> {
        let mut visited: HashSet<u32> = entries.iter().copied().collect();
        let mut candidates: BinaryHeap<Scored> = entries.iter().map(|&e| Scored(self.score(query, e), e)).collect();
        let mut best: BinaryHeap<std::cmp::Reverse<Scored>> = candidates.iter().map(|&s| std::cmp::Reverse(s)).collect();
        while let Some(current) = candidates.pop() {
            if best.len() >= ef && best.peek().is_some_and(|w| current.0 < w.0 .0) {
                break;
            }
            for &n in self.links[current.1 as usize].get(layer).into_iter().flatten() {
                if !visited.insert(n) {
                    continue;
                }
                let s = Scored(self.score(query, n), n);
                if best.len() < ef || best.peek().is_some_and(|w| s.0 > w.0 .0) {
                    candidates.push(s);
                    best.push(std::cmp::Reverse(s));
                    if best.len() > ef {
                        best.pop();
                    }
                }
            }
        }
        let mut out: Vec<Scored> = best.into_iter().map(|r| r.0).collect();
        out.sort_by(|a, b| b.cmp(a));
        out
    }

    /// Up to `limit` documents nearest to a prepared query vector, best first,
    /// with their scores. `ef` widens the graph search (ignored when exact).
    pub fn search(&self, query: &[f32], limit: usize, ef: usize) -> Vec<(Ulid, f32)> {
        let Some(mut entry) = self.entry else { return Vec::new() };
        if self.slot.len() <= EXACT_LIMIT || limit >= self.slot.len() {
            let mut all: Vec<Scored> = self.slot.values().map(|&s| Scored(self.score(query, s), s)).collect();
            all.sort_by(|a, b| b.cmp(a));
            return all.into_iter().take(limit).map(|s| (self.ids[s.1 as usize], s.0)).collect();
        }
        for layer in (1..=self.top).rev() {
            entry = self.greedy(query, entry, layer);
        }
        self.search_layer(query, &[entry], ef.max(limit), 0)
            .into_iter()
            .filter(|s| !self.deleted[s.1 as usize])
            .take(limit)
            .map(|s| (self.ids[s.1 as usize], s.0))
            .collect()
    }
}

fn random_level() -> usize {
    let r: f64 = rand::rng().random_range(f64::MIN_POSITIVE..1.0);
    ((-r.ln() / (M as f64).ln()) as usize).min(MAX_LEVEL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn random_vector(dims: usize) -> Vec<f32> {
        let mut rng = rand::rng();
        (0..dims).map(|_| rng.random_range(-1.0..1.0)).collect()
    }

    #[test]
    fn vectors_come_from_arrays_or_embedding_objects() {
        assert_eq!(extract(&json!([1, 2.5])), Some(vec![1.0, 2.5]));
        assert_eq!(extract(&json!({ "vector": [0.5], "model": "m" })), Some(vec![0.5]));
        assert_eq!(extract(&json!([1, "x"])), None);
        assert_eq!(extract(&json!([])), None);
        assert_eq!(extract(&json!("text")), None);
    }

    #[test]
    fn exact_and_graph_searches_agree() {
        let dims = 24;
        let mut store = VectorStore::new(Metric::Cosine, None);
        let ids: Vec<Ulid> = (0..6000).map(|_| Ulid::generate()).collect();
        for id in &ids {
            assert!(store.insert(*id, random_vector(dims)));
        }
        assert!(!store.insert(Ulid::generate(), vec![1.0; dims + 1]), "wrong dimensions");
        // Recall@10 of the graph against an exact scan, over a few queries.
        let mut hits = 0;
        for _ in 0..20 {
            let q = store.prepare(random_vector(dims)).unwrap();
            let exact = store.search(&q, store.len(), 0);
            let exact: HashSet<Ulid> = exact.into_iter().take(10).map(|(id, _)| id).collect();
            let graph = store.search(&q, 10, 64);
            assert!(graph.windows(2).all(|w| w[0].1 >= w[1].1), "best first");
            hits += graph.iter().filter(|(id, _)| exact.contains(id)).count();
        }
        assert!(hits >= 180, "recall {} of 200", hits);

        // Removed vectors are never returned, and enough removals rebuild the graph.
        for id in &ids[..4000] {
            store.remove(id);
        }
        assert_eq!(store.len(), 2000);
        let q = store.prepare(random_vector(dims)).unwrap();
        let removed: HashSet<&Ulid> = ids[..4000].iter().collect();
        assert!(store.search(&q, 50, 64).iter().all(|(id, _)| !removed.contains(id)));
    }

    #[test]
    fn int8_vectors_take_a_quarter_of_the_memory_and_rank_alike() {
        let dims = 64;
        let mut exact = VectorStore::new(Metric::Cosine, None);
        let mut small = VectorStore::with_quantization(Metric::Cosine, None, Quantization::Int8);
        for _ in 0..3000 {
            let (id, v) = (Ulid::generate(), random_vector(dims));
            exact.insert(id, v.clone());
            small.insert(id, v);
        }
        assert!(small.vector_bytes() * 3 < exact.vector_bytes(), "{} vs {}", small.vector_bytes(), exact.vector_bytes());
        let mut hits = 0;
        for _ in 0..20 {
            let q = exact.prepare(random_vector(dims)).unwrap();
            let truth: HashSet<Ulid> = exact.search(&q, exact.len(), 0).into_iter().take(10).map(|(id, _)| id).collect();
            hits += small.search(&q, 10, 64).iter().filter(|(id, _)| truth.contains(id)).count();
        }
        assert!(hits >= 170, "recall {} of 200", hits);
        let (codes, scale) = quantize(&[0.5, -1.0, 0.25]);
        assert_eq!(codes, [64, -127, 32]);
        assert!((scale - 1.0 / 127.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_scores_ignore_length() {
        let mut store = VectorStore::new(Metric::Cosine, Some(2));
        let (a, b) = (Ulid::generate(), Ulid::generate());
        store.insert(a, vec![10.0, 0.0]);
        store.insert(b, vec![0.0, 1.0]);
        let found = store.search(&store.prepare(vec![1.0, 0.1]).unwrap(), 2, 0);
        assert_eq!(found[0].0, a);
        assert!((found[0].1 - 0.995).abs() < 0.01);
    }
}
