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
// Sign-in failures are throttled per login and per client address: after
// `security.max_failed_logins` failures within `lockout_minutes`, further
// attempts are refused until the window passes. Each hex counts in memory,
// and failures are also shared across the lattice: the Overseer records them
// in the `_login_failures` system tessellation (replicas forward theirs), which
// replicates to every hex, so spreading attempts over hexes gains nothing. Unknown logins take as long
// as known ones (a dummy Argon2 verification), and every failure returns the
// same message, so logins can't be enumerated.
//
// Permissions: a role is a named set of permissions, and a user is granted
// roles, each on a list of tessellations (or "*" for all). Tessellation
// permissions apply to the granted tessellations; the others apply everywhere:
//   read         read documents (queries, counts, aggregations, the change feed)
//   write        insert, replace, patch, delete documents (and create the
//                tessellation by inserting into it)
//   manage       delete the tessellation and manage its indexes and schemas
//   status       server status, metrics history, lattice and storage details
//   logs         the server log
//   audit        the audit trail
//   plugins      the plugin list and delivery state
//   maintenance  flush and compact storage
//   admin        everything, including users, roles, settings and shutdown
// The built-in roles are reader (read), writer (read, write), owner (read,
// write, manage), operator (status, logs, plugins, maintenance), auditor
// (status, audit) and admin (admin). Unknown role names grant nothing.

use crate::{
    audit::AuditEvent,
    crypt::{random_bytes, verify_hash},
    engine::{EngineError, HexDBEngine},
    users::{self, RoleGrant},
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
/// System tessellation of recent sign-in failures, shared across the lattice.
pub const THROTTLE_TESSELLATION: &str = "_login_failures";

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

    fn action(self) -> Action {
        match self {
            Permission::Read => Action::Read,
            Permission::Write => Action::Write,
            Permission::Manage => Action::Manage,
        }
    }
}

/// One permission a role can hold (see the module comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Read,
    Write,
    Manage,
    Status,
    Logs,
    Audit,
    Plugins,
    Maintenance,
    Admin,
}

impl Action {
    pub const ALL: [Action; 9] = [
        Action::Read,
        Action::Write,
        Action::Manage,
        Action::Status,
        Action::Logs,
        Action::Audit,
        Action::Plugins,
        Action::Maintenance,
        Action::Admin,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Action::Read => "read",
            Action::Write => "write",
            Action::Manage => "manage",
            Action::Status => "status",
            Action::Logs => "logs",
            Action::Audit => "audit",
            Action::Plugins => "plugins",
            Action::Maintenance => "maintenance",
            Action::Admin => "admin",
        }
    }

    pub fn parse(name: &str) -> Option<Action> {
        Action::ALL.into_iter().find(|a| a.name() == name)
    }

    /// True for permissions that apply to the granted tessellations only.
    pub fn is_scoped(self) -> bool {
        matches!(self, Action::Read | Action::Write | Action::Manage)
    }

    pub fn description(self) -> &'static str {
        match self {
            Action::Read => "Read documents: get, list, query, count, aggregate, and the change feed.",
            Action::Write => "Insert, replace, patch and delete documents.",
            Action::Manage => "Delete tessellations and manage their indexes and schemas.",
            Action::Status => "View server status, metrics, storage and lattice details.",
            Action::Logs => "View the server log.",
            Action::Audit => "View the audit trail.",
            Action::Plugins => "View plugins and their delivery state.",
            Action::Maintenance => "Flush, compact and back up storage.",
            Action::Admin => "Everything, including users, roles, settings and shutdown.",
        }
    }
}

/// Role names mapped to their permissions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RoleDefinitions(pub HashMap<String, Vec<Action>>);

impl RoleDefinitions {
    /// The built-in roles alone.
    pub fn builtin() -> Self {
        RoleDefinitions(users::BUILTIN_ROLES.iter().map(|r| (r.name.to_string(), r.permissions.to_vec())).collect())
    }

    /// Resolve a user's grants. Unknown roles grant nothing.
    pub fn resolve(&self, roles: &[RoleGrant]) -> Vec<ResolvedGrant> {
        roles
            .iter()
            .filter_map(|grant| {
                let permissions = self.0.get(&grant.name)?.clone();
                Some(ResolvedGrant { role: grant.name.clone(), tessellations: grant.tessellations.clone(), permissions })
            })
            .collect()
    }
}

/// A grant with its role's permissions looked up.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResolvedGrant {
    pub role: String,
    pub tessellations: Vec<String>,
    pub permissions: Vec<Action>,
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
    /// The roles' permissions, resolved when the request was authenticated.
    pub grants: Vec<ResolvedGrant>,
    pub credential: Credential,
}

impl Principal {
    pub fn new(user_id: String, login: String, email_address: String, roles: Vec<RoleGrant>, credential: Credential, definitions: &RoleDefinitions) -> Self {
        let grants = definitions.resolve(&roles);
        Principal { user_id, login, email_address, roles, grants, credential }
    }

    pub fn is_admin(&self) -> bool {
        self.grants.iter().any(|g| g.permissions.contains(&Action::Admin))
    }

    /// True if the principal holds a permission that isn't tessellation-scoped.
    pub fn has(&self, action: Action) -> bool {
        self.is_admin() || (!action.is_scoped() && self.grants.iter().any(|g| g.permissions.contains(&action)))
    }

    /// Fail with 403 unless the principal holds `action` (see [`Principal::has`]).
    pub fn require_action(&self, action: Action) -> Result<()> {
        if self.has(action) {
            Ok(())
        } else {
            Err(forbidden(format!("This requires the '{}' permission.", action.name())))
        }
    }

    /// Every non-scoped permission held (for the UI).
    pub fn global_permissions(&self) -> Vec<Action> {
        Action::ALL.into_iter().filter(|a| !a.is_scoped() && self.has(*a)).collect()
    }

    /// True if the principal has `permission` on `tessellation`.
    pub fn can(&self, permission: Permission, tessellation: &str) -> bool {
        if self.is_admin() {
            return true;
        }
        let action = permission.action();
        self.grants.iter().any(|grant| {
            grant.permissions.contains(&action) && grant.tessellations.iter().any(|t| t == "*" || t == tessellation)
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

fn api_key_hash_with(lattice_key: &[u8; 32], secret: &str) -> String {
    let key = blake3::derive_key("HexDB 2026 API key hash v1", lattice_key);
    mac(&key, secret.as_bytes()).to_hex().to_string()
}

fn api_key_hash(engine: &HexDBEngine, secret: &str) -> Result<String> {
    Ok(api_key_hash_with(&engine.config.lattice_key()?, secret))
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
    engine.audit(&owner.login, "auth.key_create", &owner.login, serde_json::json!({ "key_id": id.to_string(), "name": name })).await;
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
    engine.audit(&by.login, "auth.key_revoke", &stored.login, serde_json::json!({ "key_id": key_id, "name": stored.name })).await;
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
        // Sessions signed with a previous lattice key stay valid during a rotation.
        let Some(claims) = engine.config.session_keys()?.iter().find_map(|key| verify_session(key, token)) else { return Ok(None) };
        if let Ok(sid) = Ulid::from_string(&claims.sid) {
            if engine.get_system_document(REVOKED_SESSIONS_TESSELLATION, sid).await?.is_some() {
                return Ok(None);
            }
        }
        let Some((user_id, user)) = users::find_by_id(engine, &claims.sub).await? else { return Ok(None) };
        if user.is_locked || claims.iat <= user.sessions_valid_after {
            return Ok(None);
        }
        let definitions = users::role_definitions(engine).await?;
        return Ok(Some(Principal::new(
            user_id.to_string(),
            user.login,
            user.email_address,
            user.roles,
            Credential::Session { session_id: claims.sid, expires_at: claims.exp },
            &definitions,
        )));
    }
    if let Some((id, secret)) = split_api_key(token) {
        let Some(doc) = engine.get_system_document(API_KEYS_TESSELLATION, id).await? else { return Ok(None) };
        let Ok(mut stored) = serde_json::from_value::<StoredApiKey>(doc.data_json()) else { return Ok(None) };
        // The hash is keyed by the lattice key; after a rotation, a key hashed
        // with a previous one is re-hashed with the current one.
        let lattice_keys = engine.config.lattice_keys()?;
        let Some(matched) = lattice_keys
            .iter()
            .position(|k| crate::crypt::constant_time_eq(api_key_hash_with(k, secret).as_bytes(), stored.secret_hash.as_bytes()))
        else {
            return Ok(None);
        };
        if matched > 0 && engine.is_writable() {
            stored.secret_hash = api_key_hash_with(&lattice_keys[0], secret);
            if let Ok(json) = serde_json::to_value(&stored) {
                if let Err(e) = engine.put_system_document(API_KEYS_TESSELLATION, id, json, None).await {
                    warn!("⚠️ Couldn't re-hash API key '{}' with the current lattice key: {:#}", stored.name, e);
                }
            }
        }
        if stored.expires > 0 && stored.expires <= Utc::now().timestamp() {
            return Ok(None);
        }
        let Some((user_id, user)) = users::find_by_id(engine, &stored.user_id).await? else { return Ok(None) };
        if user.is_locked {
            return Ok(None);
        }
        let definitions = users::role_definitions(engine).await?;
        return Ok(Some(Principal::new(
            user_id.to_string(),
            user.login,
            user.email_address,
            user.roles,
            Credential::ApiKey { key_id: id.to_string() },
            &definitions,
        )));
    }
    Ok(None)
}

/// A successful sign-in.
pub struct SignIn {
    pub token: String,
    pub claims: SessionClaims,
    pub principal: Principal,
}

/// Check a login, password and (with MFA) one-time code, and start a session.
pub async fn login(engine: &HexDBEngine, login: &str, password: &str, code: Option<&str>, client: &str) -> Result<SignIn> {
    let throttle = &engine.login_throttle;
    let login_key = format!("login:{}", login.trim().to_ascii_lowercase());
    let client_key = format!("client:{}", client);
    let blocked = match throttle.blocked(&[&login_key, &client_key]) {
        Some(wait) => Some(wait),
        None => lattice_blocked(engine, &[&login_key, &client_key]).await,
    };
    if let Some(wait) = blocked {
        warn!(%client, "🚫 Sign-in refused for '{}' from {}: too many failed attempts.", login, client);
        engine
            .record_audit(AuditEvent::new("-", "auth.login", login.trim()).outcome("denied").client(client).details(serde_json::json!({ "reason": "throttled" })))
            .await;
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
            share_throttle(engine, &[&login_key, &client_key], &[]).await;
            warn!(%client, "🚫 Failed sign-in for '{}' from {}.", login, client);
            engine
                .record_audit(AuditEvent::new("-", "auth.login", login.trim()).outcome("failed").client(client).details(serde_json::json!({ "reason": "bad_credentials" })))
                .await;
            return Err(unauthorized("Invalid login or password."));
        }
    };
    if user.is_locked {
        warn!(%client, "🚫 Sign-in to locked account '{}' from {}.", user.login, client);
        engine
            .record_audit(AuditEvent::new("-", "auth.login", &user.login).outcome("denied").client(client).details(serde_json::json!({ "reason": "locked" })))
            .await;
        return Err(unauthorized("This account is locked. Ask an administrator to unlock it."));
    }
    if user.password_expiration > 0 && user.password_expiration <= Utc::now().timestamp() {
        return Err(unauthorized("This password has expired. Ask an administrator to reset it."));
    }
    if user.use_mfa && user.mfa_secret.is_some() {
        let Some(code) = code.map(str::trim).filter(|c| !c.is_empty()) else {
            return Err(EngineError::MfaRequired("Enter the code from your authenticator app (or a backup code).".into()).into());
        };
        if !users::verify_sign_in_code(engine, user_id, code).await? {
            throttle.fail(&[&login_key, &client_key]);
            share_throttle(engine, &[&login_key, &client_key], &[]).await;
            warn!(%client, "🚫 Wrong one-time code for '{}' from {}.", user.login, client);
            engine
                .record_audit(AuditEvent::new("-", "auth.login", &user.login).outcome("failed").client(client).details(serde_json::json!({ "reason": "bad_mfa_code" })))
                .await;
            return Err(EngineError::MfaRequired("That code isn't valid. Try the current code from your authenticator app.".into()).into());
        }
    }
    throttle.succeed(&login_key);
    share_throttle(engine, &[], &[&login_key]).await;

    let (token, claims) = issue_session(&engine.config.session_key()?, &user_id.to_string(), engine.live().session_hours);
    info!(%client, "🔓 '{}' signed in from {}.", user.login, client);
    engine.record_audit(AuditEvent::new(&user.login, "auth.login", &user.login).client(client)).await;
    if engine.is_writable() {
        if let Err(e) = users::record_login(engine, user_id, client).await {
            warn!("⚠️ Couldn't record the sign-in time for '{}': {:#}", user.login, e);
        }
    }
    let definitions = users::role_definitions(engine).await?;
    Ok(SignIn {
        token,
        principal: Principal::new(
            user_id.to_string(),
            user.login,
            user.email_address,
            user.roles,
            Credential::Session { session_id: claims.sid.clone(), expires_at: claims.exp },
            &definitions,
        ),
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

/// The `_login_failures` document for a throttle key (a hash, so logins and
/// addresses aren't stored in the clear).
fn throttle_id(key: &str) -> Ulid {
    let hash = blake3::derive_key("HexDB 2026 sign-in throttle v1", key.as_bytes());
    Ulid::from(u128::from_be_bytes(hash[..16].try_into().unwrap()))
}

/// How long until any of these keys may try again, by the lattice-wide record.
async fn lattice_blocked(engine: &HexDBEngine, keys: &[&str]) -> Option<Duration> {
    if !engine.tessellation_exists(THROTTLE_TESSELLATION) {
        return None;
    }
    let window_ms = engine.login_throttle.window.as_millis() as i64;
    let max = engine.login_throttle.max_failures;
    let now = Utc::now().timestamp_millis();
    let mut wait: Option<Duration> = None;
    for key in keys {
        let Ok(Some(doc)) = engine.get_system_document(THROTTLE_TESSELLATION, throttle_id(key)).await else { continue };
        let mut times: Vec<i64> = doc.data_json()["failures"].as_array().map(|a| a.iter().filter_map(|t| t.as_i64()).collect()).unwrap_or_default();
        times.retain(|t| now - t < window_ms);
        times.sort_unstable();
        if times.len() >= max {
            let until = times[times.len() - max] + window_ms;
            let remaining = Duration::from_millis((until - now).max(1000) as u64);
            wait = Some(wait.map_or(remaining, |w| w.max(remaining)));
        }
    }
    wait
}

/// Record failures (and clear keys after a success) lattice-wide. Runs on the
/// Overseer; replicas forward to it. Never fails the sign-in.
async fn share_throttle(engine: &HexDBEngine, fail: &[&str], clear: &[&str]) {
    let result = if engine.is_writable() {
        record_throttle(engine, fail, clear).await
    } else {
        // A cleared key that was never recorded needs no round trip.
        let clear: Vec<&str> = if engine.tessellation_exists(THROTTLE_TESSELLATION) {
            let mut wanted = Vec::new();
            for key in clear {
                if matches!(engine.get_system_document(THROTTLE_TESSELLATION, throttle_id(key)).await, Ok(Some(_))) {
                    wanted.push(*key);
                }
            }
            wanted
        } else {
            Vec::new()
        };
        if fail.is_empty() && clear.is_empty() {
            return;
        }
        let body = serde_json::json!({ "fail": fail, "clear": clear });
        tokio::time::timeout(Duration::from_secs(2), crate::replication::post_to_overseer(engine, "/lattice/throttle", &body))
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out")))
    };
    if let Err(e) = result {
        warn!("⚠️ Couldn't share sign-in failures with the lattice: {:#}", e);
    }
}

/// Apply failures and clears to `_login_failures` (on the Overseer).
pub async fn record_throttle(engine: &HexDBEngine, fail: &[&str], clear: &[&str]) -> Result<()> {
    // Read-modify-write; concurrent failures must not overwrite each other.
    let _lock = engine.users_lock.lock().await;
    let window_ms = engine.login_throttle.window.as_millis() as i64;
    let cap = engine.login_throttle.max_failures * 4;
    let now = Utc::now().timestamp_millis();
    for key in fail {
        let id = throttle_id(key);
        let mut times: Vec<i64> = match engine.get_system_document(THROTTLE_TESSELLATION, id).await? {
            Some(doc) => doc.data_json()["failures"].as_array().map(|a| a.iter().filter_map(|t| t.as_i64()).collect()).unwrap_or_default(),
            None => Vec::new(),
        };
        times.retain(|t| now - t < window_ms);
        times.push(now);
        times.sort_unstable();
        let skip = times.len().saturating_sub(cap);
        let times: Vec<i64> = times.into_iter().skip(skip).collect();
        engine
            .put_system_document(THROTTLE_TESSELLATION, id, serde_json::json!({ "failures": times }), Some(now + window_ms))
            .await?;
    }
    for key in clear {
        let id = throttle_id(key);
        if engine.get_system_document(THROTTLE_TESSELLATION, id).await?.is_some() {
            engine.delete_system_document(THROTTLE_TESSELLATION, id).await?;
        }
    }
    Ok(())
}

/// Failed sign-ins per key (a login or a client address), in memory.
pub struct LoginThrottle {
    pub(crate) max_failures: usize,
    pub(crate) window: Duration,
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

    fn principal_with(roles: &[(&str, &[&str])], definitions: &RoleDefinitions) -> Principal {
        Principal::new(
            "u".into(),
            "u".into(),
            "u@x".into(),
            roles
                .iter()
                .map(|(name, tess)| RoleGrant { name: name.to_string(), tessellations: tess.iter().map(|p| p.to_string()).collect() })
                .collect(),
            Credential::ApiKey { key_id: "k".into() },
            definitions,
        )
    }

    fn principal(roles: &[(&str, &[&str])]) -> Principal {
        principal_with(roles, &RoleDefinitions::builtin())
    }

    #[test]
    fn custom_roles_grant_their_permission_sets() {
        use Permission::*;
        let mut defs = RoleDefinitions::builtin();
        defs.0.insert("appender".into(), vec![Action::Write]);
        defs.0.insert("watcher".into(), vec![Action::Status, Action::Logs, Action::Read]);
        let p = principal_with(&[("appender", &["events"]), ("watcher", &["metrics"])], &defs);
        assert!(p.can(Write, "events") && !p.can(Read, "events"), "exactly the set: write without read");
        assert!(p.can(Read, "metrics") && !p.can(Write, "metrics"));
        assert!(p.has(Action::Status) && p.has(Action::Logs));
        assert!(!p.has(Action::Audit) && !p.has(Action::Maintenance) && !p.is_admin());
        assert!(!p.has(Action::Read), "scoped permissions aren't global");
        assert!(p.require_action(Action::Plugins).is_err());
        assert_eq!(p.global_permissions(), vec![Action::Status, Action::Logs]);

        let operator = principal(&[("operator", &[])]);
        assert!(operator.has(Action::Maintenance) && operator.has(Action::Logs) && !operator.can(Read, "x"));
        let auditor = principal(&[("auditor", &[])]);
        assert!(auditor.has(Action::Audit) && !auditor.has(Action::Logs));
        let admin = principal(&[("admin", &[])]);
        assert_eq!(admin.global_permissions().len(), 6);
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
