use anyhow::Result;
use hexdb_core::{users, HexDBEngine};
use std::sync::Arc;

/// Create default roles and the configured admin user if missing, and migrate
/// users and roles stored in the old single-document format.
pub async fn init_security(engine: Arc<HexDBEngine>) -> Result<()> {
    users::bootstrap(&engine, &engine.config.security).await
}
