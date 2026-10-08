// HexDB Core Authentication and Authorization
//
// Every API request except a short public list (health, sign-in, the admin
// UI's static files, and hex-to-hex endpoints with their own credentials) must
// carry one of:
//
//   * a session token, from `POST /auth/login`, sent as the `hexdb_session`
//     cookie (the admin UI; HttpOnly, so scripts can't read it) or as
//     `Authorization: Bearer hxs.…`;
//   * an API key, `Authorization: Bearer hxk_…`, for scripts and services.
//
// Session tokens are signed (keyed BLAKE3) with a key derived from the lattice
// key, so any hex in the lattice can verify them without a database write.
// They expire after `security.session_hours`. They are revoked by signing out
// (the session ID goes into the `_revoked_sessions` system tessellation until
// it would have expired), and all of a user's sessions are revoked when the
// password changes or the user is locked or deleted (the user's
// `sessions_valid_after` moves forward). The user record is read on every
// request, so role changes and locks take effect immediately.
//
// API keys are random 256-bit secrets. Only a keyed hash is stored (in the
// `_api_keys` system tessellation), and the plaintext is shown once, at
// creation. A key acts with its user's current roles.
//
// Sign-in failures are throttled in memory per login and per client address:
// after `security.max_failed_logins` failures within `lockout_minutes`, further
// attempts are refused until the window passes. Unknown logins take as long
// as known ones (a dummy Argon2 verification), and every failure returns the
// same message, so logins can't be enumerated.
//
// Permissions: roles are granted per tessellation (`permissions` lists names,
// or "*" for all):
//   reader  read documents (queries, counts, aggregations, the change feed)
//   writer  reader + insert, replace, patch, delete documents (and create the
//           tessellation by inserting into it)
//   owner   writer + delete the tessellation and manage its indexes
//   admin   everything, including users, roles, status, logs, and plugins
// Unknown role names grant nothing.

use crate::{
    crypt::{random_bytes, verify_hash},
    engine::{EngineError, HexDBEngine},
    users::{self, RoleGrant, ADMIN_ROLE},
};
use anyhow::Result;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn};
use ulid::Ulid;

/// Cookie holding the session token for the admin UI.
pub const SESSION_COOKIE: &str = "hexdb_session";
/// System tessellation of revoked session IDs.
pub const REVOKED_SESSIONS_TESSELLATION: &str = "_revoked_sessions";
/// System tessellation of API keys (hashes only).
pub const API_KEYS_TESSELLATION: &str = "_api_keys";

const SESSION_PREFIX: &str = "hxs.";
const API_KEY_PREFIX: &str = "hxk_";

/// A valid Argon2 hash of a random password, verified when a login doesn't
/// exist so that unknown logins take as long as wrong passwords.
static DUMMY_HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn unauthorized(message: impl Into<String>) -> anyhow::Error {
    EngineError::Unauthorized(message.into()).into()
}

fn forbidden(message: impl Into<String>) -> anyhow::Error {
    EngineError::Forbidden(message.into()).into()
}

/// What a request may do to a tessellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Read,
    Write,
    /// Delete the tessellation, manage its indexes.
    Manage,
}

impl Permission {
    fn verb(self) -> &'static str {
        match self {
            Permission::Read => "read",
            Permission::Write => "write to",
            Permission::Manage => "manage",
        }
    }
}

/// How a request authenticated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Credential {
    Session { session_id: String, expires_at: i64 },
    ApiKey { key_id: String },
}

/// The authenticated user behind a request.
#[derive(Debug, Clone, Serialize)]
pub struct Principal {
    pub user_id: String,
    pub login: String,
    pub email_address: String,
    pub roles: Vec<RoleGrant>,
    pub credential: Credential,
}

impl Principal {
    pub fn is_admin(&self) -> bool {
        self.roles.iter().any(|r| r.name == ADMIN_ROLE)
    }

    /// True if the principal has `permission` on `tessellation`.
    pub fn can(&self, permission: Permission, tessellation: &str) -> bool {
        if self.is_admin() {
            return true;
        }
        self.roles.iter().any(|grant| {
            let applies = grant.permissions.iter().any(|p| p == "*" || p == tessellation);
            let allows = match grant.name.as_str() {
                "owner" => true,
                "writer" => permission != Permission::Manage,
                "reader" => permission == Permission::Read,
                _ => false,
            };
            applies && allows
        })
    }

    /// Fail with 403 unless the principal has `permission` on `tessellation`.
    pub fn require(&self, permission: Permission, tessellation: &str) -> Result<()> {
        if self.can(permission, tessellation) {
            Ok(())
        } else {
            Err(forbidden(format!("You don't have permission to {} '{}'.", permission.verb(), tessellation)))
        }
    }

    /// Fail with 403 unless the principal is an administrator.
    pub fn require_admin(&self) -> Result<()> {
        if self.is_admin() {
            Ok(())
        } else {
            Err(forbidden("This requires the admin role."))
        }
    }
}

// ---------------------------------------------------------------------------
// Session tokens
// ---------------------------------------------------------------------------

/// The signed contents of a session token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClaims {
    /// User ID.
    pub sub: String,
    /// Session ID (a ULID).
    pub sid: String,
    /// Issued at (epoch milliseconds).
    pub iat: i64,
    /// Expires at (epoch seconds).
    pub exp: i64,
}

fn mac(key: &[u8; 32], message: &[u8]) -> blake3::Hash {
    blake3::keyed_hash(key, message)
}

/// Create a signed session token for a user.
pub fn issue_session(key: &[u8; 32], user_id: &str, hours: u64) -> (String, SessionClaims) {
    let now = Utc::now();
    let claims = SessionClaims {
        sub: user_id.to_string(),
        sid: Ulid::new().to_string(),
        iat: now.timestamp_millis(),
        exp: now.timestamp() + (hours.clamp(1, 24 * 30) as i64) * 3600,
    };
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap_or_default());
    let signed = format!("{}{}", SESSION_PREFIX, payload);
    let signature = URL_SAFE_NO_PAD.encode(mac(key, signed.as_bytes()).as_bytes());
    (format!("{}.{}", signed, signature), claims)
}

/// Check a session token's signature and expiry. Says nothing about
/// revocation or the user; see [`authenticate`].
pub fn verify_session(key: &[u8; 32], token: &str) -> Option<SessionClaims> {
    if token.len() > 1024 || !token.starts_with(SESSION_PREFIX) {
        return None;
    }
    let (signed, signature) = token.rsplit_once('.')?;
    let signature: [u8; 32] = URL_SAFE_NO_PAD.decode(signature).ok()?.try_into().ok()?;
    // blake3::Hash equality is constant-time.
    if mac(key, signed.as_bytes()) != blake3::Hash::from(signature) {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(signed.strip_prefix(SESSION_PREFIX)?).ok()?;
    let claims: SessionClaims = serde_json::from_slice(&payload).ok()?;
    (claims.exp > Utc::now().timestamp()).then_some(claims)
}

// ---------------------------------------------------------------------------
// API keys
// ---------------------------------------------------------------------------

/// An API key as listed (never includes the secret).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiKeyInfo {
    pub id: String,
    pub name: String,
    pub user_id: String,
    pub login: String,
    pub created: i64,
    /// Epoch seconds, or 0 for no expiry.
    pub expires: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredApiKey {
    name: String,
    user_id: String,
    login: String,
    /// Hex keyed BLAKE3 hash of the secret.
    secret_hash: String,
    created: i64,
    #[serde(default)]
    expires: i64,
}

fn api_key_hash(engine: &HexDBEngine, secret: &str) -> Result<String> {
    let key = blake3::derive_key("HexDB 2026 API key hash v1", &engine.config.lattice_key()?);
    Ok(mac(&key, secret.as_bytes()).to_hex().to_string())
}

/// Parse `hxk_<ulid>_<secret>`.
fn split_api_key(token: &str) -> Option<(Ulid, &str)> {
    let rest = token.strip_prefix(API_KEY_PREFIX)?;
    let (id, secret) = rest.split_once('_')?;
    if secret.len() < 32 || secret.len() > 128 {
        return None;
    }
    Some((Ulid::from_string(id).ok()?, secret))
}

/// Create an API key for a user. Returns the key (shown once) and its info.
pub async fn create_api_key(engine: &HexDBEngine, owner: &Principal, name: &str, expires_in_days: Option<u64>) -> Result<(String, ApiKeyInfo)> {
    let name = name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(EngineError::Invalid("name must be 1-100 characters.".into()).into());
    }
    engine.ensure_writable()?;
    let id = Ulid::new();
    let secret = URL_SAFE_NO_PAD.encode(random_bytes(32));
    let now = Utc::now().timestamp();
    let stored = StoredApiKey {
        name: name.to_string(),
        user_id: owner.user_id.clone(),
        login: owner.login.clone(),
        secret_hash: api_key_hash(engine, &secret)?,
        created: now,
        expires: expires_in_days.map(|d| now + d.clamp(1, 3650) as i64 * 86_400).unwrap_or(0),
    };
    engine.put_system_document(API_KEYS_TESSELLATION, id, serde_json::to_value(&stored)?, None).await?;
    info!("🔑 API key '{}' created for '{}'.", name, owner.login);
    let info = ApiKeyInfo {
        id: id.to_string(),
        name: stored.name,
        user_id: stored.user_id,
        login: stored.login,
        created: stored.created,
        expires: stored.expires,
    };
    Ok((format!("{}{}_{}", API_KEY_PREFIX, id, secret), info))
}

/// API keys, all of them or one user's.
pub async fn list_api_keys(engine: &HexDBEngine, user_id: Option<&str>) -> Result<Vec<ApiKeyInfo>> {
    if !engine.tessellation_exists(API_KEYS_TESSELLATION) {
        return Ok(Vec::new());
    }
    let page = engine.list_documents(API_KEYS_TESSELLATION, None, usize::MAX).await?;
    Ok(page
        .documents
        .iter()
        .filter_map(|doc| {
            let k: StoredApiKey = serde_json::from_value(doc.data_json()).ok()?;
            (user_id.is_none_or(|u| u == k.user_id)).then(|| ApiKeyInfo {
                id: doc.id.to_string(),
                name: k.name,
                user_id: k.user_id,
                login: k.login,
                created: k.created,
                expires: k.expires,
            })
        })
        .collect())
}

/// Delete an API key. Non-admins may only delete their own. Returns false if missing.
pub async fn revoke_api_key(engine: &HexDBEngine, by: &Principal, key_id: &str) -> Result<bool> {
    let Ok(id) = Ulid::from_string(key_id) else { return Ok(false) };
    let Some(doc) = engine.get_system_document(API_KEYS_TESSELLATION, id).await? else { return Ok(false) };
    let stored: StoredApiKey = serde_json::from_value(doc.data_json())?;
    if stored.user_id != by.user_id && !by.is_admin() {
        return Ok(false);
    }
    engine.ensure_writable()?;
    engine.delete_system_document(API_KEYS_TESSELLATION, id).await?;
    info!("🔑 API key '{}' of '{}' revoked by '{}'.", stored.name, stored.login, by.login);
    Ok(true)
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// Resolve a bearer token or session cookie to a principal. `Ok(None)` means
/// the credential is not valid (expired, revoked, unknown, or the user is
/// locked or gone).
pub async fn authenticate(engine: &HexDBEngine, token: &str) -> Result<Option<Principal>> {
    let token = token.trim();
    if token.starts_with(SESSION_PREFIX) {
        let Some(claims) = verify_session(&engine.config.session_key()?, token) else { return Ok(None) };
        if let Ok(sid) = Ulid::from_string(&claims.sid) {
            if engine.get_system_document(REVOKED_SESSIONS_TESSELLATION, sid).await?.is_some() {
                return Ok(None);
            }
        }
        let Some((user_id, user)) = users::find_by_id(engine, &claims.sub).await? else { return Ok(None) };
        if user.is_locked || claims.iat <= user.sessions_valid_after {
            return Ok(None);
        }
        return Ok(Some(Principal {
            user_id: user_id.to_string(),
            login: user.login,
            email_address: user.email_address,
            roles: user.roles,
            credential: Credential::Session { session_id: claims.sid, expires_at: claims.exp },
        }));
    }
    if let Some((id, secret)) = split_api_key(token) {
        let Some(doc) = engine.get_system_document(API_KEYS_TESSELLATION, id).await? else { return Ok(None) };
        let Ok(stored) = serde_json::from_value::<StoredApiKey>(doc.data_json()) else { return Ok(None) };
        let presented = api_key_hash(engine, secret)?;
        if !crate::crypt::constant_time_eq(presented.as_bytes(), stored.secret_hash.as_bytes()) {
            return Ok(None);
        }
        if stored.expires > 0 && stored.expires <= Utc::now().timestamp() {
            return Ok(None);
        }
        let Some((user_id, user)) = users::find_by_id(engine, &stored.user_id).await? else { return Ok(None) };
        if user.is_locked {
            return Ok(None);
        }
        return Ok(Some(Principal {
            user_id: user_id.to_string(),
            login: user.login,
            email_address: user.email_address,
            roles: user.roles,
            credential: Credential::ApiKey { key_id: id.to_string() },
        }));
    }
    Ok(None)
}

/// A successful sign-in.
pub struct SignIn {
    pub token: String,
    pub claims: SessionClaims,
    pub principal: Principal,
}

/// Check a login and password and start a session.
pub async fn login(engine: &HexDBEngine, login: &str, password: &str, client: &str) -> Result<SignIn> {
    let throttle = &engine.login_throttle;
    let login_key = format!("login:{}", login.trim().to_ascii_lowercase());
    let client_key = format!("client:{}", client);
    if let Some(wait) = throttle.blocked(&[&login_key, &client_key]) {
        warn!(%client, "🚫 Sign-in refused for '{}' from {}: too many failed attempts.", login, client);
        return Err(EngineError::RateLimited(
            format!("Too many failed sign-ins. Try again in {} minute(s).", wait.as_secs().div_ceil(60).max(1)),
            wait.as_secs().max(1),
        )
        .into());
    }

    let found = if login.len() <= 64 && password.len() <= 1024 { users::find_for_login(engine, login).await? } else { None };
    let dummy = DUMMY_HASH.get_or_init(|| crate::crypt::create_hash(&URL_SAFE_NO_PAD.encode(random_bytes(16))));
    let password_ok = match &found {
        Some((_, user)) => verify_hash(password, &user.password_hash),
        None => {
            // Same cost as a real check, so unknown logins can't be detected by timing.
            let _ = verify_hash(password, dummy);
            false
        }
    };
    let (user_id, user) = match found {
        Some(found) if password_ok => found,
        _ => {
            throttle.fail(&[&login_key, &client_key]);
            warn!(%client, "🚫 Failed sign-in for '{}' from {}.", login, client);
            return Err(unauthorized("Invalid login or password."));
        }
    };
    if user.is_locked {
        warn!(%client, "🚫 Sign-in to locked account '{}' from {}.", user.login, client);
        return Err(unauthorized("This account is locked. Ask an administrator to unlock it."));
    }
    if user.password_expiration > 0 && user.password_expiration <= Utc::now().timestamp() {
        return Err(unauthorized("This password has expired. Ask an administrator to reset it."));
    }
    throttle.succeed(&login_key);

    let (token, claims) = issue_session(&engine.config.session_key()?, &user_id.to_string(), engine.config.security.session_hours);
    info!(%client, "🔓 '{}' signed in from {}.", user.login, client);
    if engine.is_writable() {
        if let Err(e) = users::record_login(engine, user_id, client).await {
            warn!("⚠️ Couldn't record the sign-in time for '{}': {:#}", user.login, e);
        }
    }
    Ok(SignIn {
        token,
        principal: Principal {
            user_id: user_id.to_string(),
            login: user.login,
            email_address: user.email_address,
            roles: user.roles,
            credential: Credential::Session { session_id: claims.sid.clone(), expires_at: claims.exp },
        },
        claims,
    })
}

/// Revoke one session (sign out). On a replica the Overseer records it.
pub async fn revoke_session(engine: &HexDBEngine, session_id: &str, expires_at: i64) -> Result<()> {
    let id = Ulid::from_string(session_id).map_err(|_| EngineError::Invalid("bad session ID".into()))?;
    let ttl = Some(expires_at.saturating_mul(1000));
    engine
        .put_system_document(REVOKED_SESSIONS_TESSELLATION, id, serde_json::json!({ "revoked": Utc::now().timestamp() }), ttl)
        .await
}

// ---------------------------------------------------------------------------
// Sign-in throttling
// ---------------------------------------------------------------------------

/// Failed sign-ins per key (a login or a client address), in memory.
pub struct LoginThrottle {
    max_failures: usize,
    window: Duration,
    failures: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl LoginThrottle {
    pub fn new(max_failures: u32, window_minutes: u64) -> Self {
        LoginThrottle {
            max_failures: max_failures.max(1) as usize,
            window: Duration::from_secs(window_minutes.max(1) * 60),
            failures: Mutex::new(HashMap::new()),
        }
    }

    /// How long until any of these keys may try again, if one is blocked.
    pub fn blocked(&self, keys: &[&str]) -> Option<Duration> {
        let now = Instant::now();
        let mut failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        let mut wait = None;
        for key in keys {
            if let Some(times) = failures.get_mut(*key) {
                while times.front().is_some_and(|t| now.duration_since(*t) >= self.window) {
                    times.pop_front();
                }
                if times.len() >= self.max_failures {
                    let until = times[times.len() - self.max_failures] + self.window;
                    let remaining = until.saturating_duration_since(now);
                    wait = Some(wait.map_or(remaining, |w: Duration| w.max(remaining)));
                }
            }
        }
        wait
    }

    pub fn fail(&self, keys: &[&str]) {
        let now = Instant::now();
        let mut failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        // Bound memory: forget keys whose failures have all aged out.
        if failures.len() > 100_000 {
            failures.retain(|_, times| times.back().is_some_and(|t| now.duration_since(*t) < self.window));
        }
        for key in keys {
            let times = failures.entry(key.to_string()).or_default();
            times.push_back(now);
            while times.len() > self.max_failures * 4 {
                times.pop_front();
            }
        }
    }

    /// A successful sign-in clears that login's failures (not the address's).
    pub fn succeed(&self, key: &str) {
        self.failures.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(roles: &[(&str, &[&str])]) -> Principal {
        Principal {
            user_id: "u".into(),
            login: "u".into(),
            email_address: "u@x".into(),
            roles: roles
                .iter()
                .map(|(name, perms)| RoleGrant { name: name.to_string(), permissions: perms.iter().map(|p| p.to_string()).collect() })
                .collect(),
            credential: Credential::ApiKey { key_id: "k".into() },
        }
    }

    #[test]
    fn roles_grant_exactly_their_permissions() {
        use Permission::*;
        let reader = principal(&[("reader", &["articles"])]);
        assert!(reader.can(Read, "articles"));
        assert!(!reader.can(Write, "articles"));
        assert!(!reader.can(Read, "orders"));

        let writer = principal(&[("writer", &["*"])]);
        assert!(writer.can(Read, "orders") && writer.can(Write, "orders"));
        assert!(!writer.can(Manage, "orders"));

        let owner = principal(&[("owner", &["orders"]), ("reader", &["articles"])]);
        assert!(owner.can(Manage, "orders") && owner.can(Write, "orders"));
        assert!(owner.can(Read, "articles") && !owner.can(Write, "articles"));

        let admin = principal(&[("admin", &[])]);
        assert!(admin.can(Manage, "anything") && admin.is_admin());
        assert!(!owner.is_admin());

        let nobody = principal(&[("custom", &["*"]), ("reader", &[])]);
        assert!(!nobody.can(Read, "articles"), "unknown roles and empty grants give nothing");
        assert!(nobody.require(Read, "x").is_err());
        assert!(nobody.require_admin().is_err());
    }

    #[test]
    fn session_tokens_are_signed_and_expire() {
        let key = [5u8; 32];
        let (token, claims) = issue_session(&key, "user-1", 1);
        assert_eq!(verify_session(&key, &token), Some(claims.clone()));
        assert!(verify_session(&[6u8; 32], &token).is_none(), "another key");

        // Tampering with the payload breaks the signature.
        let (signed, sig) = token.rsplit_once('.').unwrap();
        let forged_claims = SessionClaims { sub: "admin".into(), ..claims.clone() };
        let forged = format!("{}{}.{}", SESSION_PREFIX, URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged_claims).unwrap()), sig);
        assert!(verify_session(&key, &forged).is_none());
        assert!(verify_session(&key, &format!("{}.{}", signed, "AAAA")).is_none());
        assert!(verify_session(&key, "hxs.garbage").is_none());
        assert!(verify_session(&key, "").is_none());

        // An expired (but correctly signed) token is refused.
        let expired = SessionClaims { exp: Utc::now().timestamp() - 1, ..claims };
        let payload = format!("{}{}", SESSION_PREFIX, URL_SAFE_NO_PAD.encode(serde_json::to_vec(&expired).unwrap()));
        let token = format!("{}.{}", payload, URL_SAFE_NO_PAD.encode(mac(&key, payload.as_bytes()).as_bytes()));
        assert!(verify_session(&key, &token).is_none());
    }

    #[test]
    fn api_keys_parse_strictly() {
        let id = Ulid::new();
        let secret = "a".repeat(43);
        assert_eq!(split_api_key(&format!("hxk_{}_{}", id, secret)).map(|(i, _)| i), Some(id));
        assert!(split_api_key(&format!("hxk_{}_short", id)).is_none());
        assert!(split_api_key(&format!("hxk_notaulid_{}", secret)).is_none());
        assert!(split_api_key(&format!("{}_{}", id, secret)).is_none());
    }

    #[test]
    fn throttle_blocks_after_repeated_failures() {
        let t = LoginThrottle::new(3, 15);
        for _ in 0..2 {
            t.fail(&["login:ada", "client:1.2.3.4"]);
        }
        assert!(t.blocked(&["login:ada"]).is_none());
        t.fail(&["login:ada", "client:1.2.3.4"]);
        assert!(t.blocked(&["login:ada"]).is_some());
        assert!(t.blocked(&["login:bo", "client:1.2.3.4"]).is_some(), "the address is blocked too");
        assert!(t.blocked(&["login:bo", "client:5.6.7.8"]).is_none());
        t.succeed("login:ada");
        assert!(t.blocked(&["login:ada"]).is_none());
    }
}
