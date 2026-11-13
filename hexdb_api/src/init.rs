use anyhow::Result;
use hexdb_core::{users, EngineError, HexDBEngine};
use std::sync::Arc;
use tracing::{info, warn};

/// Create default roles and the configured admin user if missing, and migrate
/// users and roles stored in the old single-document format. Without a
/// quorum (see `replication.quorum`), or while a new Overseer collects slices,
/// nothing can be written yet; it is retried in the background until it can.
pub async fn init_security(engine: Arc<HexDBEngine>) -> Result<()> {
    match users::bootstrap(&engine, &engine.config.security).await {
        Err(e) if waits(&e) => {
            warn!("⏳ Security setup waits: {}", e);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    match users::bootstrap(&engine, &engine.config.security).await {
                        Ok(()) => return info!("🔐 Security setup finished now that this hex can write."),
                        Err(e) if waits(&e) => continue,
                        Err(e) => return warn!("⚠️ Security setup failed: {:#}", e),
                    }
                }
            });
            Ok(())
        }
        other => other,
    }
}

/// Errors that pass: no quorum yet, or an Overseer still collecting slices.
fn waits(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<EngineError>(), Some(EngineError::NoQuorum(_)) | Some(EngineError::Unavailable(_)))
}
