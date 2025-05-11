// HexDB Core Metrics Module
// This module provides functionality to collect and serialize metrics for the HexDB engine.
// It includes structures for representing various metrics such as document counts, 
// document sizes, and tessellation statistics. The metrics are collected from the engine's
// memory store and are serialized into a JSON format for easy consumption by external systems.

use crate::MemoryEngine;
use serde::Serialize;
use std::collections::HashMap;
use chrono::{DateTime, Utc};
use ulid::Ulid;

#[derive(Debug, Serialize)]
pub struct HexStatus {
    pub hex: HexMeta,
}

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
    pub vertices: Vec<Vertex>,
    pub metrics: HexMetrics,
    pub network: NetworkStatus,
}

#[derive(Debug, Serialize)]
pub struct Vertex {
    pub status: String,
    pub memory_address: String,
}

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

#[derive(Debug, Serialize)]
pub struct TessMetrics {
    pub name: String,
    pub document_count: usize,
    pub avg_document_size_bytes: usize,
    pub max_document_size_bytes: usize,
    pub min_document_size_bytes: usize,
    pub total_size_bytes: usize,
}

#[derive(Debug, Serialize)]
pub struct NetworkStatus {
    pub engine_endpoint: String,
    pub query_endpoint: String,
    pub discovery_endpoint: String,
    pub lattice: Vec<LatticeHex>,
}

#[derive(Debug, Serialize)]
pub struct LatticeHex {
    pub name: String,
    pub status: String,
    pub hex_type: String,
    pub endpoint: String,
}

pub fn collect(engine: &MemoryEngine) -> HexStatus {
    let now = Utc::now();
    let uptime = now.timestamp() - engine.start_datetime.timestamp();

    let mut total = 0;
    let mut total_bytes = 0;
    let mut min_size = usize::MAX;
    let mut max_size = 0;
    let mut tess_stats: HashMap<String, Vec<usize>> = HashMap::new();

    for doc in engine.store.iter() {
        let serialized = serde_json::to_vec(doc.value()).unwrap_or_default();
        let size = serialized.len();

        total += 1;
        total_bytes += size;
        min_size = min_size.min(size);
        max_size = max_size.max(size);

        tess_stats
            .entry(doc.value().tessellation.clone())
            .or_default()
            .push(size);
    }

    let avg = if total > 0 { total_bytes / total } else { 0 };

    let tessellations = tess_stats
        .into_iter()
        .map(|(name, sizes)| {
            let doc_count = sizes.len();
            let total = sizes.iter().sum::<usize>();
            let avg = total / doc_count;
            let max = *sizes.iter().max().unwrap_or(&0);
            let min = *sizes.iter().min().unwrap_or(&0);

            TessMetrics {
                name,
                document_count: doc_count,
                avg_document_size_bytes: avg,
                max_document_size_bytes: max,
                min_document_size_bytes: min,
                total_size_bytes: total,
            }
        })
        .collect();

    HexStatus {
        hex: HexMeta {
            id: engine.id.clone(),
            name: engine.name.clone(),
            version: engine.version.clone(),
            start_datetime: engine.start_datetime,
            uptime_seconds: uptime as u64,
            status: "healthy".to_string(),
            hex_type: engine.hex_type.clone(),
            ram_mb: engine.config.memory.ram_mb,
            disk_mb: engine.config.storage.disk_mb,
            vertices: (0..6)
                .map(|i| Vertex {
                    status: "healthy".to_string(),
                    memory_address: format!("0x{:X}", 0x1A2B3C4D + i),
                })
                .collect(),
            metrics: HexMetrics {
                total_document_count: total,
                documents_in_ram: total, // SST support not fully counted yet
                documents_on_disk: 0,
                avg_document_size_bytes: avg,
                max_document_size_bytes: max_size,
                min_document_size_bytes: if min_size == usize::MAX { 0 } else { min_size },
                total_size_bytes: total_bytes,
                tessellations,
            },
            network: NetworkStatus {
                engine_endpoint: engine.config.network.engine_endpoint.clone(),
                query_endpoint: engine.config.network.query_endpoint.clone(),
                discovery_endpoint: engine.config.network.discovery_endpoint.clone(),
                lattice: vec![], // to be added later
            },
        },
    }
}

