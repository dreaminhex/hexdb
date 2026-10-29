// HexDB Core Password Recovery
//
// A forgotten administrator password is recovered from the server's machine:
// `hexdb reset-password` generates a new password, writes a request holding
// only its Argon2 hash into the storage folder, and the running server (or
// the next one to start) gives that user the new password and writes a short
// answer (done, or why not) next to it. Writing into the storage folder
// already takes the access that reads the storage key, so this grants nothing
// new, and no network request can start it.
//
// Neither file holds a secret, so both get the folder's own permissions: the
// CLI often runs as another account than the server (a Windows service, a
// systemd user, `docker exec`), and each must read what the other wrote.
//
// The reset runs on the Overseer, where users are written. A request left on
// a replica is answered with the Overseer's address.

use crate::{audit::AuditEvent, HexDBEngine};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// How often the server looks for a request.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// What `hexdb reset-password` asks for.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResetRequest {
    pub login: String,
    /// The Argon2 hash (PHC string) of the new password.
    pub password_hash: String,
    #[serde(default)]
    pub turn_off_mfa: bool,
}

impl ResetRequest {
    /// A request with a new generated password, returned beside it.
    pub fn generate(login: &str, turn_off_mfa: bool) -> (Self, String) {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        let password = URL_SAFE_NO_PAD.encode(crate::crypt::random_bytes(18));
        let request = ResetRequest { login: login.trim().to_string(), password_hash: crate::crypt::create_hash(&password), turn_off_mfa };
        (request, password)
    }
}

/// The server's answer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResetResult {
    pub login: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn request_path(storage_dir: &Path) -> PathBuf {
    storage_dir.join("reset-password.request")
}

pub fn result_path(storage_dir: &Path) -> PathBuf {
    storage_dir.join("reset-password.result")
}

/// Ask for a reset (the CLI's side). Replaces an earlier request and removes
/// an earlier answer, so the next answer belongs to this request.
pub fn write_request(storage_dir: &Path, request: &ResetRequest) -> Result<()> {
    let _ = fs::remove_file(result_path(storage_dir));
    let path = request_path(storage_dir);
    fs::write(&path, serde_json::to_vec(request)?).with_context(|| format!("Couldn't write {}", path.display()))
}

/// Read and remove the server's answer, if there is one.
pub fn take_result(storage_dir: &Path) -> Result<Option<ResetResult>> {
    let path = result_path(storage_dir);
    let Ok(body) = fs::read(&path) else { return Ok(None) };
    let result = serde_json::from_slice(&body).with_context(|| format!("{} is unreadable", path.display()))?;
    let _ = fs::remove_file(&path);
    Ok(Some(result))
}

/// Apply a waiting request, if any (the server's side).
pub async fn apply_pending(engine: &HexDBEngine) {
    let dir = engine.config.storage_dir();
    let path = request_path(&dir);
    let Ok(body) = fs::read(&path) else { return };
    // Until an election settles, nobody can say where users are written: keep the request.
    if !engine.is_writable() && engine.overseer_endpoint.read().unwrap().is_none() {
        return;
    }
    if let Err(e) = fs::remove_file(&path) {
        // A request that can't be removed would be applied again and again.
        warn!("⚠️ A password reset was requested, but {} can't be removed, so it was not applied: {}", path.display(), e);
        return;
    }
    let result = match serde_json::from_slice::<ResetRequest>(&body) {
        Err(e) => ResetResult { login: String::new(), error: Some(format!("The request is unreadable: {}", e)) },
        Ok(request) => reset(engine, &request).await,
    };
    let saved = serde_json::to_vec(&result).map_err(anyhow::Error::from).and_then(|body| Ok(fs::write(result_path(&dir), body)?));
    if let Err(e) = saved {
        warn!("⚠️ Couldn't write the password reset's answer to {}: {:#}", result_path(&dir).display(), e);
    }
}

async fn reset(engine: &HexDBEngine, request: &ResetRequest) -> ResetResult {
    let login = request.login.trim();
    match crate::users::recover_password(engine, login, &request.password_hash, request.turn_off_mfa).await {
        Ok(login) => {
            warn!("🔑 The password of '{}' was reset from the server's machine (hexdb reset-password); its sessions were signed out.", login);
            engine
                .record_audit(
                    AuditEvent::new("-", "user.password_recovered", &login)
                        .client("local")
                        .details(serde_json::json!({ "mfa_turned_off": request.turn_off_mfa })),
                )
                .await;
            // Failed sign-ins before the reset don't count against the new password.
            let key = format!("login:{}", login.to_ascii_lowercase());
            engine.login_throttle.succeed(&key);
            if let Err(e) = crate::auth::record_throttle(engine, &[], &[&key]).await {
                debug!("Couldn't clear the sign-in failures of '{}': {:#}", login, e);
            }
            ResetResult { login, error: None }
        }
        Err(e) => {
            info!("🔑 A password reset for '{}' was refused: {:#}", login, e);
            ResetResult { login: login.to_string(), error: Some(format!("{:#}", e)) }
        }
    }
}

/// Look for reset requests while the server runs.
pub fn spawn_recovery_task(engine: Arc<HexDBEngine>, shutdown_rx: watch::Receiver<()>) {
    crate::tasks::supervise("password recovery", shutdown_rx.clone(), move || {
        let (engine, mut shutdown_rx) = (engine.clone(), shutdown_rx.clone());
        async move {
            loop {
                apply_pending(&engine).await;
                tokio::select! {
                    _ = shutdown_rx.changed() => break,
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_holds_only_the_hash_of_the_new_password() {
        let dir = std::env::temp_dir().join(format!("hexdb-recovery-{}", ulid::Ulid::generate()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(result_path(&dir), br#"{"login":"old"}"#).unwrap();
        let (request, password) = ResetRequest::generate(" admin ", true);
        write_request(&dir, &request).unwrap();
        assert!(take_result(&dir).unwrap().is_none(), "a new request clears the last answer");

        let stored = fs::read_to_string(request_path(&dir)).unwrap();
        assert!(!stored.contains(&password), "the password itself is never written");
        let stored: ResetRequest = serde_json::from_str(&stored).unwrap();
        assert_eq!(stored.login, "admin");
        assert!(crate::crypt::verify_hash(&password, &stored.password_hash));
        let _ = fs::remove_dir_all(&dir);
    }
}
