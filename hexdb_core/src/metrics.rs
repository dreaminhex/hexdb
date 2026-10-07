// HexDB Core Metrics Module
// This module provides functionality to collect and serialize metrics for the HexDB engine.
// It includes structures for representing various metrics such as document counts,
// document sizes, and tessellation statistics. The metrics are collected from the engine's
// memory store and are serialized into a JSON format for easy consumption by external systems.

use crate::HexDBEngine;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    sync::atomic::Ordering,
};
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
    pub network: NetworkMetrics,
}

/// Represents the metadata of a vertex in the HexDB engine.
#[derive(Debug, Serialize)]
pub struct VertexMeta {
    pub status: String,
    pub memory_address: String,
}

/// Represents the metrics of the HexDB engine, including document counts and sizes.
#[derive(Debug, Serialize)]
pub struct HexMetrics {
    pub total_document_count: usize,
    pub documents_in_ram: usize,
    pub documents_on_disk: usize,
    pub avg_document_size_bytes: usize,
    pub max_document_size_bytes: usize,
    pub min_document_size_bytes: usize,
    pub total_size_bytes: usize,
    pub tessellations: Vec<TessMetrics>,
}

/// Represents the metrics of a tessellation in the HexDB engine.
#[derive(Debug, Serialize)]
pub struct TessMetrics {
    pub name: String,
    pub document_count: usize,
    pub avg_document_size_bytes: usize,
    pub max_document_size_bytes: usize,
    pub min_document_size_bytes: usize,
    pub total_size_bytes: usize,
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
    let uptime = now.timestamp() - engine.start_datetime.timestamp();

    let node = engine.node.lock().await;
    let ram_doc_count = node.total_document_count;
    let ram_bytes = node.calculate_total_ram_bytes();

    let disk_doc_count = engine.sst.count_documents_on_disk().unwrap_or(0);
    let disk_bytes = engine.sst.total_doc_bytes.load(Ordering::Relaxed);

    let mut seen_keys = HashSet::new();
    let mut tess_sizes: HashMap<String, Vec<usize>> = HashMap::new();

    for vertex in &node.vertices {
        for ((tess, doc_id), (chunk, _hash, is_full)) in &vertex.storage {
            if *is_full {
                let key = (tess.clone(), doc_id.clone());
                if seen_keys.insert(key.clone()) {
                    let size = chunk.len();
                    tess_sizes.entry(tess.clone()).or_default().push(size);
                }
            }
        }
    }

    let (mut total_bytes, mut min_size, mut max_size) = (0, usize::MAX, 0);

    let tessellations: Vec<TessMetrics> = tess_sizes
        .into_iter()
        .map(|(name, sizes)| {
            let count = sizes.len();
            let sum = sizes.iter().sum::<usize>();
            let avg = if count > 0 { sum / count } else { 0 };
            let max = *sizes.iter().max().unwrap_or(&0);
            let min = *sizes.iter().min().unwrap_or(&0);

            total_bytes += sum;
            min_size = min_size.min(min);
            max_size = max_size.max(max);

            TessMetrics {
                name,
                document_count: count,
                avg_document_size_bytes: avg,
                max_document_size_bytes: max,
                min_document_size_bytes: min,
                total_size_bytes: sum,
            }
        })
        .collect();

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
        id: engine.id.clone(),
        name: engine.name.clone(),
        version: engine.version.clone(),
        start_datetime: engine.start_datetime,
        uptime_seconds: uptime as u64,
        status: "healthy".to_string(),
        hex_type: engine.hex_type.clone(),
        ram_mb: engine.config.memory.ram_mb,
        disk_mb: engine.config.storage.disk_mb,
        vertices: node
            .vertices
            .iter()
            .map(|v| {
                let ptr = v as *const crate::vertex::Vertex as usize;
                VertexMeta {
                    status: "healthy".to_string(),
                    memory_address: format!("0x{:X}", ptr),
                }
            })
            .collect(),
        metrics: HexMetrics {
            total_document_count: ram_doc_count + disk_doc_count,
            documents_in_ram: ram_doc_count,
            documents_on_disk: disk_doc_count,
            avg_document_size_bytes: if ram_doc_count > 0 {
                ram_bytes / ram_doc_count
            } else {
                0
            },
            max_document_size_bytes: max_size,
            min_document_size_bytes: if min_size == usize::MAX { 0 } else { min_size },
            total_size_bytes: ram_bytes + disk_bytes,
            tessellations,
        },
        network: NetworkMetrics {
            api_endpoint: engine.config.network.api_endpoint.clone(),
            query_endpoint: engine.config.network.query_endpoint.clone(),
            discovery_endpoint: engine.config.network.discovery_endpoint.clone(),
            lattice: LatticeMetrics {
                name: engine.config.network.lattice_name.clone(),
                hexes: lattice_hexes.clone(),
            },
        },
    }
}
