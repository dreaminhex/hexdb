use anyhow::{bail, Result};
use chrono::Utc;
use hexdb_core::{create_hash, HexDBEngine};
use tracing::{info};
use std::sync::Arc;
use serde_json::json;

pub async fn init_security(engine: Arc<HexDBEngine>) -> Result<()> {

    // Add all default roles if they don't exist
    let roles_exist = engine.count_documents("roles").await.unwrap_or(0) > 0;

    if !roles_exist {

        engine.create_tessellation("roles", "system")?;
        {

            let roles = json!({
                "roles": [
                    { "name": "admin", "description": "Full system access." },
                    { "name": "reader", "description": "Read-only access to specified tessellations." },
                    { "name": "writer", "description": "Write access to specified tessellations (assumes read access)." },
                    { "name": "owner", "description": "Full access to specified tessellations." }
                ]
            });

            if let Err(e) = engine.insert_json("roles", roles, None).await {
                bail!("❗ Failed to insert initial roles document: {}.", e);
            } else {
                info!("✅ Roles tessellation initialized.");
            }
        }
    }

    // Ensure the configuration is set for the admin user.
    // This is a one-time setup step. The admin user is created if it doesn't exist.
    let config = &engine.config;
    let login = config.security.admin_login.trim();
    let password = config.security.admin_password.trim();
    let email = config.security.admin_email.trim();

    if login.is_empty() || password.is_empty() || email.is_empty() {
        bail!("❌ Admin user configuration is incomplete. All fields (login, password, email) must be set.");
    }

    // Create the admin user if it doesn't exist.
    let users_exist = engine.count_documents("users").await.unwrap_or(0) > 0;

    if !users_exist {

        engine.create_tessellation("users", "system")?;
        {

            let hashed = create_hash(password);

            let user = json!({
                "users": [{
                    "login": login,
                    "password": hashed,
                    "created": Utc::now().timestamp(),
                    "last_login": 0,
                    "login_ips": [],
                    "is_locked": false,
                    "password_attempts": 0,
                    "email_address": email,
                    "password_reset": false,
                    "password_reset_token": null,
                    "password_reset_expiry": 0,
                    "last_password_change": Utc::now().timestamp(),
                    "password_expiration": -1,
                    "use_mfa": false,
                    "mfa_secret": null,
                    "mfa_backup_codes": [],
                    "roles": [{
                        "name": "admin",
                        "permissions": ["*"]
                    }]
                }]
            });

            if let Err(e) = engine.insert_json("users", user, None).await {
                bail!("❗ Failed to insert initial admin user document: {}.", e);
            } else {
                info!("✅ Admin user added.");
            }
        }
    }

    Ok(())
}
