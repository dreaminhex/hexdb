// HexDB Core Background Tasks
// Periodic maintenance: flushing to SSTables, compaction, TTL sweeps, and
// vertex integrity checks. Each task stops when the shutdown channel fires.

use crate::HexDBEngine;
use std::{future::Future, sync::Arc, time::Duration};
use tokio::sync::watch;
use tracing::{debug, error, warn};

/// Run a background task, and start it again (1 s later, doubling to 60 s
/// while it keeps panicking) if it panics, until the hex shuts down. `start`
/// makes a fresh copy of the task each time. A task that returns is done.
/// Restarts are counted in `/status` (`restarts.tasks`, see `crate::crashes`).
pub fn supervise<F, Fut>(name: impl Into<String>, shutdown_rx: watch::Receiver<()>, start: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let name = name.into();
    tokio::spawn(async move {
        let mut delay = Duration::from_secs(1);
        loop {
            crate::crashes::task_started(&name);
            let started = std::time::Instant::now();
            let outcome = tokio::spawn(start()).await;
            let panic = match outcome {
                Ok(()) => break,
                Err(e) if e.is_cancelled() => break,
                Err(e) => {
                    let payload = e.into_panic();
                    payload.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| payload.downcast_ref::<String>().cloned()).unwrap_or_else(|| "(no message)".into())
                }
            };
            crate::crashes::task_panicked(&name, &panic);
            // Shutting down (or the sender is gone): don't restart.
            if shutdown_rx.has_changed().unwrap_or(true) {
                break;
            }
            if started.elapsed() > Duration::from_secs(300) {
                delay = Duration::from_secs(1);
            }
            error!("❌ The {} task panicked ({}); restarting it in {} s.", name, panic, delay.as_secs());
            let mut wait_rx = shutdown_rx.clone();
            tokio::select! {
                _ = wait_rx.changed() => break,
                _ = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(Duration::from_secs(60));
            warn!("♻️  Restarting the {} task.", name);
        }
        crate::crashes::task_stopped(&name);
    });
}

/// Flush unflushed writes to SSTables every `interval`, or sooner when enough
/// data accumulates. Also keeps memory use within budget.
pub fn spawn_flush_task(engine: Arc<HexDBEngine>, interval: Duration, shutdown_rx: watch::Receiver<()>) {
    crate::tasks::supervise("flush", shutdown_rx.clone(), move || {
        let (engine, mut shutdown_rx) = (engine.clone(), shutdown_rx.clone());
        async move {
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
        }
    });
}

/// Compact SSTables, and remove file content nothing references, every `interval`.
pub fn spawn_compaction_task(engine: Arc<HexDBEngine>, interval: Duration, shutdown_rx: watch::Receiver<()>) {
    crate::tasks::supervise("compaction", shutdown_rx.clone(), move || {
        let (engine, mut shutdown_rx) = (engine.clone(), shutdown_rx.clone());
        async move {
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
                        if let Err(e) = engine.collect_file_garbage().await {
                            error!("❌ File garbage collection failed: {:#}", e);
                        }
                    }
                }
            }
        }
    });
}

/// Evict expired documents from memory every `interval`.
pub fn spawn_ttl_sweep_task(engine: Arc<HexDBEngine>, interval: Duration, shutdown_rx: watch::Receiver<()>) {
    crate::tasks::supervise("TTL sweep", shutdown_rx.clone(), move || {
        let (engine, mut shutdown_rx) = (engine.clone(), shutdown_rx.clone());
        async move {
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
        }
    });
}

/// Extract metadata from uploaded files (see `crate::extract`), and copy file
/// content from other hexes.
pub fn spawn_extraction_task(engine: Arc<HexDBEngine>, shutdown_rx: watch::Receiver<()>) {
    let (e, rx) = (engine.clone(), shutdown_rx.clone());
    supervise("extraction", shutdown_rx.clone(), move || e.clone().run_extraction(rx.clone()));
    let (e, rx) = (engine.clone(), shutdown_rx.clone());
    supervise("AI files", shutdown_rx.clone(), move || e.clone().run_ai_files(rx.clone()));
    let rx = shutdown_rx.clone();
    supervise("file sync", shutdown_rx, move || engine.clone().run_file_sync(rx.clone()));
}

/// Verify every shard in the chunk store and repair bad ones every `interval`.
pub fn spawn_scrub_task(engine: Arc<HexDBEngine>, interval: Duration, shutdown_rx: watch::Receiver<()>) {
    crate::tasks::supervise("scrub", shutdown_rx.clone(), move || {
        let (engine, mut shutdown_rx) = (engine.clone(), shutdown_rx.clone());
        async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        debug!("🛑 Scrub task is shutting down...");
                        break;
                    }
                    _ = tokio::time::sleep(interval) => {
                        if let Err(e) = engine.scrub_files().await {
                            error!("❌ Scrub failed: {:#}", e);
                        }
                    }
                }
            }
        }
    });
}

/// Verify vertex shards and repair corrupt ones every `interval`.
pub fn spawn_vertex_monitoring_task(engine: Arc<HexDBEngine>, interval: Duration, shutdown_rx: watch::Receiver<()>) {
    crate::tasks::supervise("vertex monitoring", shutdown_rx.clone(), move || {
        let (engine, mut shutdown_rx) = (engine.clone(), shutdown_rx.clone());
        async move {
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
        }
    });
}

/// Record a metrics sample now and then every `interval`, for the dashboard's charts.
pub fn spawn_metrics_task(engine: Arc<HexDBEngine>, interval: Duration, shutdown_rx: watch::Receiver<()>) {
    crate::tasks::supervise("metrics", shutdown_rx.clone(), move || {
        let (engine, mut shutdown_rx) = (engine.clone(), shutdown_rx.clone());
        async move {
            let mut taken: u64 = 0;
            loop {
                engine.refresh_disk_usage().await;
                let sample = crate::metrics::sample(&engine).await;
                engine.history.push(sample);
                taken += 1;
                // Save about once a minute (and at shutdown; see HexDBEngine::shutdown).
                if taken.is_multiple_of(4) {
                    if let Err(e) = engine.save_metrics_history() {
                        tracing::warn!("Couldn't save the metrics history: {:#}", e);
                    }
                }
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        debug!("🛑 Metrics task is shutting down...");
                        break;
                    }
                    _ = tokio::time::sleep(interval) => {}
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn a_panicking_task_is_restarted_until_shutdown() {
        let runs = Arc::new(AtomicU32::new(0));
        let (tx, rx) = watch::channel(());
        let counted = runs.clone();
        supervise("test panics once", rx.clone(), move || {
            let runs = counted.clone();
            async move {
                if runs.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("first run");
                }
            }
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while runs.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(runs.load(Ordering::SeqCst), 2, "restarted once, then it finished");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let health = crate::crashes::tasks().into_iter().find(|t| t.name == "test panics once").unwrap();
        assert_eq!(health.restarts, 1);
        assert_eq!(health.last_panic.as_deref(), Some("first run"));
        assert!(!health.running);

        // Shutting down: a panic isn't restarted.
        let runs = Arc::new(AtomicU32::new(0));
        let counted = runs.clone();
        tx.send(()).unwrap();
        supervise("test panics at shutdown", rx, move || {
            let runs = counted.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                panic!("always");
            }
        });
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }
}
