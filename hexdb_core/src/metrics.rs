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
    pub network: NetworkMetrics,
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
}

/// Represents a single Hex node participating in the lattice.
#[derive(Debug, Serialize, Clone)]
pub struct LatticeHex {
    pub name: String,
    pub role: String,
    pub status: String,
    pub ip: String,
}

// Represents a hex node in the lattice with its metrics.
#[derive(Debug, Serialize)]
pub struct LatticeMetric {
    pub name: String,
    pub status: String,
    pub hex_type: String,
    pub endpoint: String,
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
    pub query_endpoint: String,
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

    let mut lattice_hexes: Vec<LatticeHex> = Vec::new();

    // Include self
    lattice_hexes.push(LatticeHex {
        name: engine.name.clone(),
        role: engine.hex_type.clone(),
        status: "active".to_string(),
        ip: engine.config.network.discovery_endpoint.clone(),
    });

    // Include peers
    let peer_list = engine.peers.lock().await;
    for peer in peer_list.iter() {
        lattice_hexes.push(LatticeHex {
            name: peer.name.clone(),
            role: peer.role.clone(),
            status: "active".to_string(),
            ip: peer.ip.clone(),
        });
    }

    HexMeta {
        id: engine.id,
        name: engine.name.clone(),
        version: engine.version.clone(),
        start_datetime: engine.start_datetime,
        uptime_seconds: uptime as u64,
        status: "healthy".to_string(),
        hex_type: engine.hex_type.clone(),
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
        },
        network: NetworkMetrics {
            api_endpoint: engine.config.network.api_endpoint.clone(),
            query_endpoint: engine.config.network.query_endpoint.clone(),
            discovery_endpoint: engine.config.network.discovery_endpoint.clone(),
            lattice: LatticeMetrics {
                name: engine.config.network.lattice_name.clone(),
                hexes: lattice_hexes,
            },
        },
    }
}
