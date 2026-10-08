use anyhow::Result;
use hexdb_core::{users, EngineError, HexDBEngine};
use std::sync::Arc;
use tracing::{info, warn};

/// Create default roles and the configured admin user if missing, and migrate
/// users and roles stored in the old single-document format. Without a
/// quorum (see `replication.quorum`) nothing can be written yet; it is retried
/// in the background until the lattice is big enough.
pub async fn init_security(engine: Arc<HexDBEngine>) -> Result<()> {
    match users::bootstrap(&engine, &engine.config.security).await {
        Err(e) if matches!(e.downcast_ref::<EngineError>(), Some(EngineError::NoQuorum(_))) => {
            warn!("⏳ Security setup waits for a quorum: {}", e);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    match users::bootstrap(&engine, &engine.config.security).await {
                        Ok(()) => return info!("🔐 Security setup finished now that the lattice has a quorum."),
                        Err(e) if matches!(e.downcast_ref::<EngineError>(), Some(EngineError::NoQuorum(_))) => continue,
                        Err(e) => return warn!("⚠️ Security setup failed: {:#}", e),
                    }
                }
            });
            Ok(())
        }
        other => other,
    }
}
