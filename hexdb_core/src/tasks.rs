// HexDB Core Background Tasks
// Periodic maintenance: flushing to SSTables, compaction, TTL sweeps, and
// vertex integrity checks. Each task stops when the shutdown channel fires.

use crate::HexDBEngine;
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;
use tracing::{debug, error};

/// Flush unflushed writes to SSTables every `interval`, or sooner when enough
/// data accumulates. Also keeps memory use within budget.
pub fn spawn_flush_task(engine: Arc<HexDBEngine>, interval: Duration, mut shutdown_rx: watch::Receiver<()>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    debug!("🛑 Flush task is shutting down...");
                    break;
                }
                _ = tokio::time::sleep(interval) => {}
                _ = engine.flush_needed().notified() => {}
            }
            if let Err(e) = engine.flush().await {
                error!("❌ Flush failed: {:#}", e);
            }
            engine.enforce_memory_budget().await;
        }
    });
}

/// Compact SSTables every `interval`.
pub fn spawn_compaction_task(engine: Arc<HexDBEngine>, interval: Duration, mut shutdown_rx: watch::Receiver<()>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    debug!("🛑 SST compaction task is shutting down...");
                    break;
                }
                _ = tokio::time::sleep(interval) => {
                    if let Err(e) = engine.compact().await {
                        error!("❌ Compaction failed: {:#}", e);
                    }
                }
            }
        }
    });
}

/// Evict expired documents from memory every `interval`.
pub fn spawn_ttl_sweep_task(engine: Arc<HexDBEngine>, interval: Duration, mut shutdown_rx: watch::Receiver<()>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    debug!("🛑 TTL sweep task is shutting down...");
                    break;
                }
                _ = tokio::time::sleep(interval) => {
                    engine.sweep_expired().await;
                }
            }
        }
    });
}

/// Verify vertex shards and repair corrupt ones every `interval`.
pub fn spawn_vertex_monitoring_task(engine: Arc<HexDBEngine>, interval: Duration, mut shutdown_rx: watch::Receiver<()>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    debug!("🛑 Vertex monitoring task is shutting down...");
                    break;
                }
                _ = tokio::time::sleep(interval) => {
                    engine.check_vertices().await;
                }
            }
        }
    });
}
