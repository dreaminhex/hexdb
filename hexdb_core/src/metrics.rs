// HexDB Core Metrics Module
// This module provides functionality to collect and serialize metrics for the HexDB engine.
// It includes structures for representing various metrics such as document counts,
// document sizes, and tessellation statistics. The metrics are collected from the engine's
// memory store and SSTables and are serialized into a JSON format for easy consumption
// by external systems.

use crate::HexDBEngine;
use chrono::{DateTime, Utc};
use serde::Serialize;
use ulid::Ulid;

/// Represents the metadata of a HexDB engine instance.
#[derive(Debug, Serialize)]
pub struct HexMeta {
    pub id: Ulid,
    pub name: String,
    pub start_datetime: DateTime<Utc>,
    pub uptime_seconds: u64,
    pub version: String,
    pub status: String,
    pub hex_type: String,
    pub ram_mb: u64,
    pub disk_mb: u64,
    pub vertices: Vec<VertexMeta>,
    pub metrics: HexMetrics,
    pub storage: StorageMetrics,
    pub operations: OperationMetrics,
    pub network: NetworkMetrics,
    /// This hex's replication (see `ReplicationStatus`), with its lag.
    pub replication: serde_json::Value,
}

/// Represents the metadata of a vertex in the HexDB engine.
#[derive(Debug, Serialize)]
pub struct VertexMeta {
    pub id: usize,
    /// "healthy", or "repaired" if corrupt shards were found and rebuilt since startup.
    pub status: String,
    pub bytes: usize,
    pub shards: usize,
    pub corrupt_shards_found: u64,
    pub shards_repaired: u64,
}

/// Represents the metrics of the HexDB engine, including document counts and sizes.
#[derive(Debug, Serialize)]
pub struct HexMetrics {
    /// Visible documents across all tessellations.
    pub total_document_count: usize,
    /// Visible documents whose newest version is held in memory.
    pub documents_in_ram: usize,
    /// Visible documents only on disk (not currently cached).
    pub documents_on_disk: usize,
    pub avg_document_size_bytes: usize,
    pub max_document_size_bytes: usize,
    pub min_document_size_bytes: usize,
    /// Memory used by vertex shards plus SSTable bytes on disk.
    pub total_size_bytes: usize,
    pub tessellations: Vec<TessMetrics>,
}

/// Represents the metrics of a tessellation in the HexDB engine.
/// Sizes are uncompressed for documents in memory and compressed for documents only on disk.
#[derive(Debug, Serialize)]
pub struct TessMetrics {
    pub name: String,
    pub kind: String,
    pub document_count: usize,
    pub documents_in_ram: usize,
    pub documents_on_disk: usize,
    pub avg_document_size_bytes: usize,
    pub max_document_size_bytes: usize,
    pub min_document_size_bytes: usize,
    pub total_size_bytes: usize,
}

/// Storage engine internals.
#[derive(Debug, Serialize)]
pub struct StorageMetrics {
    pub ram_budget_bytes: usize,
    pub memory_bytes: usize,
    pub memory_entries: usize,
    pub unflushed_entries: usize,
    pub unflushed_bytes: usize,
    pub disk_bytes: u64,
    pub sstable_files: usize,
    pub next_sequence: u64,
    /// SSTables not yet encrypted with the current key (written before
    /// encryption, or with a previous key). Compaction rewrites them; at 0, the
    /// previous keys can be removed from the configuration.
    pub sstable_files_on_old_keys: usize,
}

/// Operation counters since startup.
#[derive(Debug, Serialize)]
pub struct OperationMetrics {
    pub reads_total: u64,
    pub writes_total: u64,
    pub queries_total: u64,
}

/// A hex in the lattice, as seen from this hex.
#[derive(Debug, Serialize, Clone)]
pub struct LatticeHex {
    pub id: String,
    pub name: String,
    pub role: String,
    /// "active", or "lost" when a peer stopped answering discovery.
    pub status: String,
    /// Discovery address.
    pub ip: String,
    pub api_endpoint: String,
    /// Role preference: auto, overseer, harvester or replicant.
    pub preference: String,
    /// Highest sequence number the hex has applied.
    pub last_seq: u64,
    /// When discovery last heard from it (None for this hex).
    pub last_seen: Option<DateTime<Utc>>,
    pub is_self: bool,
    /// leading (Overseer), streaming, syncing, waiting, or error.
    pub replication_state: String,
    /// Overseer sequence number applied.
    pub applied_seq: u64,
    /// Changes behind the Overseer (replicas only, when known).
    pub lag: Option<u64>,
}

/// Report structure for the current lattice state.
#[derive(Debug, Serialize, Clone)]
pub struct LatticeMetrics {
    pub name: String,
    pub hexes: Vec<LatticeHex>,
}

/// Network metrics for the HexDB engine.
#[derive(Debug, Serialize)]
pub struct NetworkMetrics {
    pub api_endpoint: String,
    pub discovery_endpoint: String,
    pub lattice: LatticeMetrics,
}

/// Collects metrics from the HexDB engine and returns a `HexMeta` structure.
pub async fn collect(engine: &HexDBEngine) -> HexMeta {
    let now = Utc::now();
    let uptime = (now - engine.start_datetime).num_seconds().max(0);
    let stats = engine.stats().await;

    let mut tessellations = Vec::new();
    let mut all_sizes: Vec<usize> = Vec::new();
    let (mut total_docs, mut in_ram, mut on_disk) = (0, 0, 0);

    for (name, kind) in engine.tessellations() {
        let t = engine.tessellation_stats(&name).await;
        total_docs += t.document_count;
        in_ram += t.documents_in_memory;
        on_disk += t.documents_on_disk_only;
        let sum: usize = t.sizes.iter().sum();
        tessellations.push(TessMetrics {
            name,
            kind,
            document_count: t.document_count,
            documents_in_ram: t.documents_in_memory,
            documents_on_disk: t.documents_on_disk_only,
            avg_document_size_bytes: if t.sizes.is_empty() { 0 } else { sum / t.sizes.len() },
            max_document_size_bytes: t.sizes.iter().copied().max().unwrap_or(0),
            min_document_size_bytes: t.sizes.iter().copied().min().unwrap_or(0),
            total_size_bytes: sum,
        });
        all_sizes.extend(t.sizes);
    }

    // This hex first, then peers.
    let me = crate::network::discovery::local_identity(engine).await;
    let mut lattice_hexes: Vec<LatticeHex> = vec![LatticeHex {
        id: me.id,
        name: me.name,
        role: me.role,
        status: "active".to_string(),
        ip: me.ip,
        api_endpoint: me.api_endpoint,
        preference: me.preference,
        last_seq: me.last_seq,
        last_seen: None,
        is_self: true,
        replication_state: me.replication_state,
        applied_seq: me.applied_seq,
        lag: None,
    }];
    for member in engine.peers.lock().await.iter() {
        lattice_hexes.push(LatticeHex {
            id: member.hex.id.clone(),
            name: member.hex.name.clone(),
            role: member.hex.role.clone(),
            status: member.status.clone(),
            ip: member.hex.ip.clone(),
            api_endpoint: member.hex.api_endpoint.clone(),
            preference: member.hex.preference.clone(),
            last_seq: member.hex.last_seq,
            last_seen: Some(member.last_seen),
            is_self: false,
            replication_state: member.hex.replication_state.clone(),
            applied_seq: member.hex.applied_seq,
            lag: None,
        });
    }
    // Lag relative to the Overseer (this hex's own view of it is freshest when it leads).
    if let Some(head) = lattice_hexes.iter().find(|h| h.role == crate::network::discovery::ROLE_OVERSEER && h.status == "active").map(|h| h.applied_seq) {
        for hex in lattice_hexes.iter_mut().filter(|h| h.role != crate::network::discovery::ROLE_OVERSEER && h.status == "active") {
            if matches!(hex.replication_state.as_str(), "streaming" | "syncing") {
                hex.lag = Some(head.saturating_sub(hex.applied_seq));
            }
        }
    }

    HexMeta {
        id: engine.id,
        name: engine.name.clone(),
        version: engine.version.clone(),
        start_datetime: engine.start_datetime,
        uptime_seconds: uptime as u64,
        status: "healthy".to_string(),
        hex_type: engine.role(),
        ram_mb: engine.config.memory.ram_mb,
        disk_mb: engine.config.storage.disk_mb,
        vertices: stats
            .vertices
            .iter()
            .map(|v| VertexMeta {
                id: v.id,
                status: if v.corrupt_found > 0 { "repaired" } else { "healthy" }.to_string(),
                bytes: v.bytes,
                shards: v.shards,
                corrupt_shards_found: v.corrupt_found,
                shards_repaired: v.repaired,
            })
            .collect(),
        metrics: HexMetrics {
            total_document_count: total_docs,
            documents_in_ram: in_ram,
            documents_on_disk: on_disk,
            avg_document_size_bytes: if all_sizes.is_empty() { 0 } else { all_sizes.iter().sum::<usize>() / all_sizes.len() },
            max_document_size_bytes: all_sizes.iter().copied().max().unwrap_or(0),
            min_document_size_bytes: all_sizes.iter().copied().min().unwrap_or(0),
            total_size_bytes: stats.memory_bytes + stats.disk_bytes as usize,
            tessellations,
        },
        storage: StorageMetrics {
            ram_budget_bytes: stats.ram_budget_bytes,
            memory_bytes: stats.memory_bytes,
            memory_entries: stats.memory_entries,
            unflushed_entries: stats.dirty_entries,
            unflushed_bytes: stats.dirty_bytes,
            disk_bytes: stats.disk_bytes,
            sstable_files: stats.sst_files,
            next_sequence: stats.next_seq,
            sstable_files_on_old_keys: engine.sst_files_needing_rewrite().await,
        },
        operations: OperationMetrics {
            reads_total: stats.reads_total,
            writes_total: stats.writes_total,
            queries_total: stats.queries_total,
        },
        network: NetworkMetrics {
            api_endpoint: engine.config.network.api_endpoint.clone(),
            discovery_endpoint: engine.config.network.discovery_endpoint.clone(),
            lattice: LatticeMetrics {
                name: engine.config.network.lattice_name.clone(),
                hexes: lattice_hexes,
            },
        },
        replication: {
            let status = engine.replication.lock().unwrap().clone();
            let mut value = serde_json::to_value(&status).unwrap_or_default();
            if engine.is_writable() {
                value["state"] = "leading".into();
                value["applied_seq"] = engine.changes.published_seq().into();
            } else {
                value["lag"] = status.lag().into();
            }
            value
        },
    }
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

/// How often the metrics task records a sample.
pub const HISTORY_INTERVAL_SECONDS: u64 = 15;
/// How long samples are kept (6 hours).
pub const HISTORY_RETENTION_SECONDS: u64 = 6 * 60 * 60;

/// One point-in-time metrics sample.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct MetricsSample {
    pub timestamp: DateTime<Utc>,
    /// Visible documents per user tessellation.
    pub documents: std::collections::BTreeMap<String, usize>,
    pub memory_bytes: usize,
    pub disk_bytes: u64,
    pub unflushed_entries: usize,
    /// Cumulative counters since startup; differences between samples give rates.
    pub reads_total: u64,
    pub writes_total: u64,
    pub queries_total: u64,
}

/// A bounded series of metrics samples. Saved to `metrics-history.hxe` in the
/// data directory (encrypted) every minute and at shutdown, and reloaded at
/// startup.
pub struct MetricsHistory {
    samples: std::sync::Mutex<std::collections::VecDeque<MetricsSample>>,
    capacity: usize,
}

impl Default for MetricsHistory {
    fn default() -> Self {
        let capacity = (HISTORY_RETENTION_SECONDS / HISTORY_INTERVAL_SECONDS) as usize;
        MetricsHistory { samples: std::sync::Mutex::new(std::collections::VecDeque::with_capacity(capacity)), capacity }
    }
}

impl MetricsHistory {
    pub fn push(&self, sample: MetricsSample) {
        let mut samples = self.samples.lock().unwrap();
        if samples.len() == self.capacity {
            samples.pop_front();
        }
        samples.push_back(sample);
    }

    /// Every sample, oldest first.
    pub fn all(&self) -> Vec<MetricsSample> {
        self.samples.lock().unwrap().iter().cloned().collect()
    }

    /// Replace the series (at startup), dropping samples older than the retention.
    pub fn restore(&self, mut samples: Vec<MetricsSample>) {
        let cutoff = Utc::now() - chrono::Duration::seconds(HISTORY_RETENTION_SECONDS as i64);
        samples.retain(|s| s.timestamp >= cutoff);
        let skip = samples.len().saturating_sub(self.capacity);
        *self.samples.lock().unwrap() = samples.into_iter().skip(skip).collect();
    }

    /// The newest sample.
    pub fn last(&self) -> Option<MetricsSample> {
        self.samples.lock().unwrap().back().cloned()
    }

    /// Samples taken at or after `since`, oldest first.
    pub fn since(&self, since: DateTime<Utc>) -> Vec<MetricsSample> {
        self.samples.lock().unwrap().iter().filter(|s| s.timestamp >= since).cloned().collect()
    }
}

/// Take a metrics sample (document counts scan each tessellation's indexes).
pub async fn sample(engine: &HexDBEngine) -> MetricsSample {
    let stats = engine.stats().await;
    let mut documents = std::collections::BTreeMap::new();
    for (name, kind) in engine.tessellations() {
        if kind == "user" {
            documents.insert(name.clone(), engine.count_documents(&name).await.unwrap_or(0));
        }
    }
    MetricsSample {
        timestamp: Utc::now(),
        documents,
        memory_bytes: stats.memory_bytes,
        disk_bytes: stats.disk_bytes,
        unflushed_entries: stats.dirty_entries,
        reads_total: stats.reads_total,
        writes_total: stats.writes_total,
        queries_total: stats.queries_total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> MetricsSample {
        MetricsSample {
            timestamp: DateTime::from_timestamp(seconds, 0).unwrap(),
            documents: Default::default(),
            memory_bytes: 0,
            disk_bytes: 0,
            unflushed_entries: 0,
            reads_total: 0,
            writes_total: 0,
            queries_total: 0,
        }
    }

    #[test]
    fn history_is_bounded_and_filters_by_time() {
        let history = MetricsHistory { samples: Default::default(), capacity: 3 };
        for s in 0..5 {
            history.push(at(s * 10));
        }
        let all = history.since(DateTime::from_timestamp(0, 0).unwrap());
        assert_eq!(all.iter().map(|s| s.timestamp.timestamp()).collect::<Vec<_>>(), vec![20, 30, 40]);
        assert_eq!(history.since(DateTime::from_timestamp(35, 0).unwrap()).len(), 1);
    }
}
