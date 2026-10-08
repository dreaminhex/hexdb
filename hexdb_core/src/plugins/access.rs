// API access for plugins (`[access]` in the manifest): ingest sources and
// enrichers write documents through the API like any client. Each such plugin
// gets its own user, `plugin-<id>`, holding exactly the role the manifest
// asks for, and a fresh API key every time it starts (older keys are revoked).
// The key reaches the process as HEXDB_API_KEY; it is never stored in plaintext.

use super::Manifest;
use crate::{
    auth::{self, Credential, Principal},
    engine::HexDBEngine,
    users::{self, RoleGrant},
};
use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AccessConfig {
    /// The role the plugin acts with, e.g. "writer" or a custom role.
    pub role: String,
    /// Tessellations the role applies to ("*" for all).
    #[serde(default)]
    pub tessellations: Vec<String>,
}

/// The login of a plugin's user.
pub fn login_for(plugin_id: &str) -> String {
    let slug: String = plugin_id
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let mut slug = slug.split('-').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("-");
    slug.truncate(56);
    format!("plugin-{}", slug)
}

/// A new API key for the plugin's user (created or updated to the manifest's role).
pub async fn api_key(engine: &HexDBEngine, manifest: &Manifest, access: &AccessConfig) -> Result<String> {
    let login = login_for(&manifest.id);
    let grant = RoleGrant { name: access.role.clone(), tessellations: access.tessellations.clone() };
    let (user_id, user) = users::ensure_service_user(engine, &login, vec![grant]).await?;
    // Revoke the keys of earlier runs.
    let definitions = users::role_definitions(engine).await?;
    let principal = Principal::new(user_id.clone(), user.login.clone(), user.email_address.clone(), user.roles.clone(), Credential::ApiKey { key_id: String::new() }, &definitions);
    for key in auth::list_api_keys(engine, Some(&user_id)).await? {
        auth::revoke_api_key(engine, &principal, &key.id).await?;
    }
    let (key, _) = auth::create_api_key(engine, &principal, &format!("plugin {}", manifest.id), None).await?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    #[test]
    fn logins_are_safe_slugs() {
        assert_eq!(super::login_for("@sources/postgres"), "plugin-sources-postgres");
        assert_eq!(super::login_for("My Plugin!!"), "plugin-my-plugin");
    }
}
