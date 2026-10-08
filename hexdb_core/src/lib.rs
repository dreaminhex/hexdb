pub mod config;
pub mod logging;
pub mod engine;
pub mod hex;
pub mod document;
pub mod wal;
pub mod sst;
pub mod catalog;
pub mod filter;
pub mod index;
pub mod aggregate;
pub mod changes;
pub mod replication;
pub mod plugins;
pub mod auth;
pub mod metrics;
pub mod crypt;
pub mod vertex;
pub mod tasks;
pub mod runtime;
pub mod users;
pub mod network {
    pub mod discovery;
    pub mod lattice_auth;
}

pub use config::{load_config, load_config_from, HexConfig};
pub use runtime::{RuntimeInfo, local_base_url, SHUTDOWN_TOKEN_HEADER};
pub use logging::{init_logging, log_buffer, parse_level, LogQuery, LogRecord, LOG_CAPACITY};
pub use engine::{
    DocumentQuery, EngineError, EngineStats, FlushStats, HexDBEngine, HexIdentity, IdempotencyKey, ListPage, Outcome,
    QueryPage, TessellationStats,
    UpdateSummary, IDEMPOTENCY_TESSELLATION, MAX_BULK_ITEMS,
    parse_transaction, TransactionResult, TxOpKind, TxOperation, TxResult, MAX_TRANSACTION_OPS,
};
pub use document::{project, Document, FieldValue};
pub use hex::Hex;
pub use vertex::Vertex;
pub use wal::{WalOp, WalRecord};
pub use metrics::{HexMeta, HexMetrics, VertexMeta, TessMetrics, NetworkMetrics, LatticeMetrics, StorageMetrics, collect};
pub use crypt::{constant_time_eq, create_hash, decode_encryption_key, random_bytes, read_sealed_file, verify_hash, write_sealed_file, KeyRing};
pub use sst::{SstFile, SstStore};
pub use catalog::{validate_tessellation_name, TessellationInfo};
pub use filter::{tokenize, Filter, SortKey};
pub use index::{IndexDef, IndexInfo, IndexKind};
pub use engine::REPLICATION_TESSELLATION;
pub use plugins::{spawn_plugins, PluginStatus};
pub use auth::{Permission, Principal, SESSION_COOKIE};
pub use replication::{spawn_replication_task, ReplicationStatus, LATTICE_SIGNATURE_HEADER};
pub use changes::{Change, ChangeFeed, ChangeKind, HistoryExpired, CHANGE_HISTORY};
pub use aggregate::{parse_aggregates, AggregateOp, AggregateResult, AggregateSpec, Aggregation};
pub use tasks::{spawn_vertex_monitoring_task, spawn_ttl_sweep_task, spawn_flush_task, spawn_compaction_task, spawn_metrics_task};
pub use network::discovery::{
    discover_peers, discovery_round, elect, local_identity, parse_preference, spawn_discovery_task, start_discovery_listener,
    LatticeMember, PeerHex, ROLE_HARVESTER, ROLE_OVERSEER, ROLE_REPLICANT,
};
