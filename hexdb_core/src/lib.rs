pub mod config;
pub mod logging;
pub mod engine;
pub mod hex;
pub mod document;
pub mod wal;
pub mod sst;
pub mod catalog;
pub mod filter;
pub mod metrics;
pub mod crypt;
pub mod vertex;
pub mod tasks;
pub mod runtime;
pub mod users;
pub mod network {
    pub mod discovery;
}

pub use config::{load_config, load_config_from, HexConfig};
pub use runtime::{RuntimeInfo, local_base_url, SHUTDOWN_TOKEN_HEADER};
pub use logging::init_logging;
pub use engine::{
    DocumentQuery, EngineError, EngineStats, FlushStats, HexDBEngine, HexIdentity, IdempotencyKey, ListPage, Outcome,
    QueryPage, TessellationStats,
    UpdateSummary, IDEMPOTENCY_TESSELLATION, MAX_BULK_ITEMS,
};
pub use document::{Document, FieldValue};
pub use hex::Hex;
pub use vertex::Vertex;
pub use wal::{WalOp, WalRecord};
pub use metrics::{HexMeta, HexMetrics, VertexMeta, TessMetrics, NetworkMetrics, LatticeMetrics, StorageMetrics, collect};
pub use crypt::{create_hash, verify_hash, decode_encryption_key, constant_time_eq};
pub use sst::{SstFile, SstStore};
pub use catalog::{validate_tessellation_name, TessellationInfo};
pub use filter::{Filter, SortKey};
pub use tasks::{spawn_vertex_monitoring_task, spawn_ttl_sweep_task, spawn_flush_task, spawn_compaction_task, spawn_metrics_task};
pub use network::discovery::{PeerHex, discover_peers, start_discovery_listener};
