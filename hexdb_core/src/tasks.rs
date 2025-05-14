use crate::{wal_writer_task, HexConfig, HexDBEngine, Wal};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{mpsc::{Receiver, Sender}, watch};
use tracing::{debug, error};

/// This function spawns a task that writes the Write-Ahead Log (WAL) to a file.
/// It uses AES-GCM for encryption and Zstandard for compression.
pub fn spawn_wal_writer_task(
    config: HexConfig,
    wal_rx: Receiver<Wal>,
    wal_path: PathBuf,
    key: Arc<Vec<u8>>
) {
    tokio::spawn(wal_writer_task(config, wal_rx, wal_path.clone(), key.clone()));
    debug!("📓 WAL writer task started (path: {:?})", wal_path);    
}

/// This function spawns a task that compacts the SSTables in the memory engine.
/// It runs at a specified interval and attempts to compact all SSTables.
pub fn spawn_compaction_task(
    engine: Arc<HexDBEngine>,
    interval: Duration,
    mut shutdown_rx: watch::Receiver<()>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    debug!("🛑 SST compaction task is shutting down...");
                    break;
                }
                _ = tokio::time::sleep(interval) => {
                    debug!("🗜️  Compacting SSTables...");
                    if let Err(e) = engine.sst.compact().await {
                        error!("❌ Compaction error: {}.", e);
                    }
                }
            }
        }
    });
}

/// This function spawns a task that flushes the Write-Ahead Log (WAL) to SSTables.
/// It checks the size of the WAL and triggers a flush if it exceeds a certain threshold.
pub fn spawn_flush_task(
    engine: Arc<HexDBEngine>,
    wal_tx: Sender<Wal>,
    interval: Duration,
    mut shutdown_rx: watch::Receiver<()>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    debug!("🛑 WAL flush task is shutting down...");
                    break;
                }
                _ = tokio::time::sleep(interval) => {
                    let doc_count = engine.node.lock().await.count_total_documents();
                    if doc_count > engine.adaptive_max_docs().await {
                        debug!("💾 Flushing WAL to SSTables...");
                        engine.sst.flush(engine.node.clone(), &wal_tx).await;
                    }
                }
            }
        }
    });
}

/// This function spawns a task that sweeps expired documents from the memory engine.
/// It checks for documents that have exceeded their TTL (Time To Live) and removes them.
pub fn spawn_ttl_sweep_task(
    engine: Arc<HexDBEngine>,
    interval: Duration,
    mut shutdown_rx: watch::Receiver<()>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    debug!("🛑 TTL sweep task is shutting down...");
                    break;
                }
                _ = tokio::time::sleep(interval) => {
                    debug!("🧹 Sweeping expired documents...");
                    if let Err(e) = engine.sweep_expired_documents().await {
                        error!("❌ TTL sweep failed: {}.", e);
                    }
                }
            }
        }
    });
}

/// This function spawns a task that monitors the vertices in the memory engine.
/// It checks the validity of vertex chunks and repairs any corrupt chunks found.
pub fn spawn_vertex_monitoring_task(
    engine: Arc<HexDBEngine>,
    interval: Duration,
    mut shutdown_rx: watch::Receiver<()>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    debug!("🛑 Vertex monitoring task is shutting down...");
                    break;
                }
                _ = tokio::time::sleep(interval) => {
                    debug!("🔍 Checking integrity of vertex chunks...");

                    let node = engine.node.lock().await;
                    let mut keys = vec![];
                    for tess in node.tessellations.keys() {
                        for (id, _) in node.get_all_docs(tess) {
                            keys.push((tess.clone(), id));
                        }
                    }
                    drop(node);

                    for (tess, id) in keys {
                        let node = engine.node.lock().await;
                        let results = node.validate_vertex_chunks(&tess, &id);
                        drop(node);

                        let invalid = results.iter().filter(|(_, ok)| !ok).count();
                        if invalid > 0 {
                            debug!("🩹 Repairing {} invalid chunks for {}:{}", invalid, tess, id);
                            let mut node = engine.node.lock().await;
                            node.repair_corrupt_chunks(&tess, &id);
                        }
                    }
                }
            }
        }
    });
}
