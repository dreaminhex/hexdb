// HexDB Core Users and Roles
//
// Users and roles are stored as documents in the `users` and `roles` system
// tessellations, one document per user or role. This module validates input,
// hashes passwords with Argon2, keeps logins unique (ignoring case), protects
// the last administrator, and never returns password hashes or MFA secrets.
//
// A role is a named permission set (see `crate::auth::Action`). The built-in
// roles can't be changed or deleted; custom roles can. A user holds grants:
// a role name plus the tessellations it applies to.
//
// Earlier builds stored all users in one document holding a `users` array and
// all roles in one document holding a `roles` array; `bootstrap` migrates those.

use crate::{
    auth::{Action, RoleDefinitions},
    config::SecurityConfig,
    crypt::create_hash,
    engine::{EngineError, IdempotencyKey, Outcome},
    Document, HexDBEngine,
};
use anyhow::{anyhow, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};
use ulid::Ulid;

pub const USERS_TESSELLATION: &str = "users";
pub const ROLES_TESSELLATION: &str = "roles";
pub const ADMIN_ROLE: &str = "admin";

/// A role that always exists.
pub struct BuiltinRole {
    pub name: &'static str,
    pub description: &'static str,
    pub permissions: &'static [Action],
}

pub const BUILTIN_ROLES: &[BuiltinRole] = &[
    BuiltinRole { name: "admin", description: "Full system access.", permissions: &[Action::Admin] },
    BuiltinRole { name: "reader", description: "Read the granted tessellations.", permissions: &[Action::Read] },
    BuiltinRole { name: "writer", description: "Read and write the granted tessellations.", permissions: &[Action::Read, Action::Write] },
    BuiltinRole {
        name: "owner",
        description: "Read, write and manage (indexes, schemas, deletion) the granted tessellations.",
        permissions: &[Action::Read, Action::Write, Action::Manage],
    },
    BuiltinRole {
        name: "operator",
        description: "Run the server: status, logs, plugins, flush and compaction. No document access.",
        permissions: &[Action::Status, Action::Logs, Action::Plugins, Action::Maintenance],
    },
    BuiltinRole { name: "auditor", description: "Review the audit trail and server status. No document access.", permissions: &[Action::Status, Action::Audit] },
];

fn builtin(name: &str) -> Option<&'static BuiltinRole> {
    BUILTIN_ROLES.iter().find(|r| r.name == name)
}
const MIN_PASSWORD_LEN: usize = 12;
const MAX_PASSWORD_LEN: usize = 1024;

/// A role granted to a user, with the tessellations it applies to ("*" for all).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleGrant {
    pub name: String,
    /// Accepted as `permissions` too, the name used by earlier builds.
    #[serde(default, alias = "permissions")]
    pub tessellations: Vec<String>,
}

/// A user as stored. Never returned by the API; see [`UserView`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredUser {
    pub login: String,
    #[serde(default)]
    pub email_address: String,
    #[serde(alias = "password")]
    pub password_hash: String,
    #[serde(default)]
    pub roles: Vec<RoleGrant>,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub last_login: i64,
    #[serde(default)]
    pub login_ips: Vec<String>,
    #[serde(default)]
    pub is_locked: bool,
    #[serde(default)]
    pub password_attempts: u32,
    #[serde(default)]
    pub password_reset: bool,
    #[serde(default)]
    pub password_reset_token: Option<String>,
    #[serde(default)]
    pub password_reset_expiry: i64,
    #[serde(default)]
    pub last_password_change: i64,
    #[serde(default = "never")]
    pub password_expiration: i64,
    #[serde(default)]
    pub use_mfa: bool,
    #[serde(default)]
    pub mfa_secret: Option<String>,
    /// Argon2 hashes of the unused backup codes.
    #[serde(default)]
    pub mfa_backup_codes: Vec<String>,
    /// A secret being enrolled, until a code confirms it.
    #[serde(default)]
    pub mfa_pending_secret: Option<String>,
    /// The last TOTP step used to sign in (codes for it and earlier are refused).
    #[serde(default)]
    pub mfa_last_step: i64,
    /// Sessions issued at or before this time (epoch milliseconds) are no longer valid.
    #[serde(default)]
    pub sessions_valid_after: i64,
    /// The login in lowercase, indexed for case-insensitive lookups.
    #[serde(default)]
    pub login_key: String,
}

/// Name of the internal index on `users.login_key`.
const LOGIN_INDEX: &str = "login_key";

fn login_key(login: &str) -> String {
    login.trim().to_ascii_lowercase()
}

fn never() -> i64 {
    -1
}

impl StoredUser {
    fn is_active_admin(&self) -> bool {
        !self.is_locked && self.roles.iter().any(|r| r.name == ADMIN_ROLE)
    }
}

/// A user as returned by the API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserView {
    pub id: String,
    pub login: String,
    pub email_address: String,
    pub roles: Vec<RoleGrant>,
    pub created: i64,
    pub last_login: i64,
    pub is_locked: bool,
    pub use_mfa: bool,
    pub last_password_change: i64,
    pub password_expiration: i64,
}

impl UserView {
    fn new(id: Ulid, user: &StoredUser) -> Self {
        UserView {
            id: id.to_string(),
            login: user.login.clone(),
            email_address: user.email_address.clone(),
            roles: user.roles.clone(),
            created: user.created,
            last_login: user.last_login,
            is_locked: user.is_locked,
            use_mfa: user.use_mfa,
            last_password_change: user.last_password_change,
            password_expiration: user.password_expiration,
        }
    }
}

/// Input for creating a user.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewUser {
    pub login: String,
    pub password: String,
    #[serde(alias = "email")]
    pub email_address: String,
    #[serde(default)]
    pub roles: Vec<RoleGrant>,
}

/// Input for changing a user. For a full replace (PUT), `email_address` and
/// `roles` are required; a new `password` and `login` are optional.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserChanges {
    pub login: Option<String>,
    pub password: Option<String>,
    #[serde(alias = "email")]
    pub email_address: Option<String>,
    pub roles: Option<Vec<RoleGrant>>,
    pub is_locked: Option<bool>,
    pub use_mfa: Option<bool>,
    pub password_expiration: Option<i64>,
}

/// A role as returned by the API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub permissions: Vec<Action>,
    /// Built-in roles can't be changed or deleted.
    pub builtin: bool,
}

/// Input for creating or changing a custom role.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleInput {
    pub name: Option<String>,
    pub description: Option<String>,
    pub permissions: Option<Vec<String>>,
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    EngineError::Invalid(message.into()).into()
}

fn validate_login(login: &str) -> Result<()> {
    if login.is_empty() || login.len() > 64 {
        return Err(invalid("login must be 1-64 characters long."));
    }
    if !login.chars().all(|c| c.is_ascii_alphanumeric() || "._@-".contains(c)) {
        return Err(invalid("login may only contain letters, digits, '.', '_', '@' and '-'."));
    }
    Ok(())
}

/// Passwords that are long enough but far too common.
const COMMON_PASSWORDS: &[&str] = &[
    "password1234", "123456789012", "qwertyuiopas", "passwordpassword", "letmein12345", "administrator",
    "hexdbadmin1234", "changeme1234", "welcome12345", "iloveyou1234", "111111111111", "abcdefghijkl",
];

/// Password policy for new and changed passwords: 12-1024 characters, not
/// containing the login, not one repeated character, not a common password.
fn validate_password_for(password: &str, login: &str) -> Result<()> {
    let len = password.chars().count();
    if !(MIN_PASSWORD_LEN..=MAX_PASSWORD_LEN).contains(&len) {
        return Err(invalid(format!(
            "password must be {}-{} characters long.",
            MIN_PASSWORD_LEN, MAX_PASSWORD_LEN
        )));
    }
    let lower = password.to_lowercase();
    if !login.is_empty() && lower.contains(&login.to_lowercase()) {
        return Err(invalid("password must not contain the login."));
    }
    let first = password.chars().next();
    if password.chars().all(|c| Some(c) == first) || COMMON_PASSWORDS.contains(&lower.as_str()) {
        return Err(invalid("password is too easy to guess; choose a longer or less common one."));
    }
    Ok(())
}

fn validate_email(email: &str) -> Result<()> {
    let ok = email.len() <= 254
        && !email.chars().any(char::is_whitespace)
        && email.split_once('@').is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty());
    if !ok {
        return Err(invalid("email_address is not a valid email address."));
    }
    Ok(())
}

fn parse_user(doc: &Document) -> Option<StoredUser> {
    serde_json::from_value(doc.data_json()).ok()
}

async fn all_users(engine: &HexDBEngine) -> Result<Vec<(Ulid, StoredUser)>> {
    let page = engine.list_documents(USERS_TESSELLATION, None, usize::MAX).await?;
    Ok(page
        .documents
        .iter()
        .filter_map(|doc| parse_user(doc).map(|u| (doc.id, u)))
        .collect())
}

/// A user by ID only.
pub(crate) async fn find_by_id(engine: &HexDBEngine, id: &str) -> Result<Option<(Ulid, StoredUser)>> {
    let Ok(id) = Ulid::from_string(id) else { return Ok(None) };
    if !engine.tessellation_exists(USERS_TESSELLATION) {
        return Ok(None);
    }
    Ok(engine.get_system_document(USERS_TESSELLATION, id).await?.and_then(|doc| parse_user(&doc).map(|u| (doc.id, u))))
}

/// A user by login only (ignoring case), for sign-in.
pub(crate) async fn find_for_login(engine: &HexDBEngine, login: &str) -> Result<Option<(Ulid, StoredUser)>> {
    find_by_login(engine, login).await
}

/// Note a successful sign-in (time and client address, last 10 addresses).
pub(crate) async fn record_login(engine: &HexDBEngine, id: Ulid, client: &str) -> Result<()> {
    let _lock = engine.users_lock.lock().await;
    let Some((_, mut user)) = find_by_id(engine, &id.to_string()).await? else { return Ok(()) };
    user.last_login = Utc::now().timestamp();
    user.login_ips.retain(|ip| ip != client);
    user.login_ips.insert(0, client.to_string());
    user.login_ips.truncate(10);
    engine.replace_document(USERS_TESSELLATION, &id.to_string(), to_json(&user)?, None, None).await?;
    Ok(())
}

/// Change your own password: the current one must be given. All other
/// sessions are signed out.
pub async fn change_own_password(engine: &HexDBEngine, user_id: &str, current: &str, new: &str) -> Result<()> {
    let _lock = engine.users_lock.lock().await;
    let (id, mut user) = find_by_id(engine, user_id).await?.ok_or_else(|| not_found(user_id))?;
    if !crate::crypt::verify_hash(current, &user.password_hash) {
        return Err(EngineError::Forbidden("The current password is wrong.".into()).into());
    }
    validate_password_for(new, &user.login)?;
    user.password_hash = create_hash(new);
    user.last_password_change = Utc::now().timestamp();
    user.sessions_valid_after = Utc::now().timestamp_millis();
    engine.replace_document(USERS_TESSELLATION, &id.to_string(), to_json(&user)?, None, None).await?;
    info!("🔑 '{}' changed their password; other sessions were signed out.", user.login);
    Ok(())
}

// ---------------------------------------------------------------------------
// Multi-factor authentication (see crate::mfa)
// ---------------------------------------------------------------------------

fn check_password(user: &StoredUser, password: &str) -> Result<()> {
    if crate::crypt::verify_hash(password, &user.password_hash) {
        Ok(())
    } else {
        Err(EngineError::Forbidden("The current password is wrong.".into()).into())
    }
}

/// Start enrolling: a new pending secret. Returns (secret, otpauth URI).
pub async fn mfa_setup(engine: &HexDBEngine, user_id: &str, password: &str) -> Result<(String, String)> {
    engine.ensure_writable()?;
    let _lock = engine.users_lock.lock().await;
    let (id, mut user) = find_by_id(engine, user_id).await?.ok_or_else(|| not_found(user_id))?;
    check_password(&user, password)?;
    if user.use_mfa {
        return Err(EngineError::Conflict("MFA is already on. Turn it off first to enrol a new authenticator.".into()).into());
    }
    let secret = crate::mfa::new_secret();
    user.mfa_pending_secret = Some(secret.clone());
    engine.replace_document(USERS_TESSELLATION, &id.to_string(), to_json(&user)?, None, None).await?;
    let issuer = format!("HexDB {}", engine.config.network.lattice_name);
    Ok((secret.clone(), crate::mfa::otpauth_uri(&issuer, &user.login, &secret)))
}

/// Finish enrolling with a code from the app. Returns the backup codes (shown once).
pub async fn mfa_enable(engine: &HexDBEngine, user_id: &str, code: &str) -> Result<Vec<String>> {
    engine.ensure_writable()?;
    let _lock = engine.users_lock.lock().await;
    let (id, mut user) = find_by_id(engine, user_id).await?.ok_or_else(|| not_found(user_id))?;
    let secret = user.mfa_pending_secret.clone().ok_or_else(|| invalid("Start with POST /auth/mfa/setup."))?;
    let step = crate::mfa::verify_totp(&secret, code, Utc::now().timestamp(), 0)
        .ok_or_else(|| invalid("That code isn't valid. Check the time on your device and try the current code."))?;
    let (codes, hashes) = crate::mfa::new_backup_codes();
    user.use_mfa = true;
    user.mfa_secret = Some(secret);
    user.mfa_pending_secret = None;
    user.mfa_backup_codes = hashes;
    user.mfa_last_step = step;
    // Other sessions were signed in with the password alone.
    user.sessions_valid_after = Utc::now().timestamp_millis();
    engine.replace_document(USERS_TESSELLATION, &id.to_string(), to_json(&user)?, None, None).await?;
    info!("🔐 '{}' turned on multi-factor authentication.", user.login);
    Ok(codes)
}

/// Turn MFA off (password and a current code or backup code).
pub async fn mfa_disable(engine: &HexDBEngine, user_id: &str, password: &str, code: &str) -> Result<()> {
    engine.ensure_writable()?;
    let _lock = engine.users_lock.lock().await;
    let (id, mut user) = find_by_id(engine, user_id).await?.ok_or_else(|| not_found(user_id))?;
    check_password(&user, password)?;
    if !user.use_mfa {
        return Ok(());
    }
    if !second_factor_matches(engine, id, &mut user, code)? {
        return Err(EngineError::Forbidden("That code isn't valid.".into()).into());
    }
    user.use_mfa = false;
    user.mfa_secret = None;
    user.mfa_backup_codes.clear();
    engine.replace_document(USERS_TESSELLATION, &id.to_string(), to_json(&user)?, None, None).await?;
    info!("🔐 '{}' turned off multi-factor authentication.", user.login);
    Ok(())
}

/// New backup codes, replacing the old ones (password and a current code).
pub async fn mfa_new_backup_codes(engine: &HexDBEngine, user_id: &str, password: &str, code: &str) -> Result<Vec<String>> {
    engine.ensure_writable()?;
    let _lock = engine.users_lock.lock().await;
    let (id, mut user) = find_by_id(engine, user_id).await?.ok_or_else(|| not_found(user_id))?;
    check_password(&user, password)?;
    if !user.use_mfa || !second_factor_matches(engine, id, &mut user, code)? {
        return Err(EngineError::Forbidden("MFA is off, or that code isn't valid.".into()).into());
    }
    let (codes, hashes) = crate::mfa::new_backup_codes();
    user.mfa_backup_codes = hashes;
    engine.replace_document(USERS_TESSELLATION, &id.to_string(), to_json(&user)?, None, None).await?;
    Ok(codes)
}

/// Check a TOTP or backup code, updating `user` (last step, used backup
/// code) without saving it. Backup codes need a writable hex.
fn second_factor_matches(engine: &HexDBEngine, id: Ulid, user: &mut StoredUser, code: &str) -> Result<bool> {
    let Some(secret) = user.mfa_secret.clone() else { return Ok(false) };
    let last = {
        let steps = engine.mfa_steps.lock().unwrap();
        steps.get(&id).copied().unwrap_or(0).max(user.mfa_last_step)
    };
    if let Some(step) = crate::mfa::verify_totp(&secret, code, Utc::now().timestamp(), last) {
        engine.mfa_steps.lock().unwrap().insert(id, step);
        user.mfa_last_step = step;
        return Ok(true);
    }
    if code.trim().len() == 11 {
        if !engine.is_writable() {
            return Err(EngineError::ReadOnly(
                "Backup codes can only be used on the Overseer (they are used up when signing in). Use your authenticator app, or sign in on the Overseer.".into(),
            )
            .into());
        }
        if let Some(i) = crate::mfa::match_backup_code(&user.mfa_backup_codes, code) {
            user.mfa_backup_codes.remove(i);
            return Ok(true);
        }
    }
    Ok(false)
}

/// The second factor at sign-in. `Ok(true)` if the code is valid (and the
/// user record was updated where possible), `Ok(false)` if it isn't.
pub(crate) async fn verify_sign_in_code(engine: &HexDBEngine, id: Ulid, code: &str) -> Result<bool> {
    let _lock = engine.users_lock.lock().await;
    let Some((_, mut user)) = find_by_id(engine, &id.to_string()).await? else { return Ok(false) };
    let backup_before = user.mfa_backup_codes.len();
    if !second_factor_matches(engine, id, &mut user, code)? {
        return Ok(false);
    }
    if engine.is_writable() {
        engine.replace_document(USERS_TESSELLATION, &id.to_string(), to_json(&user)?, None, None).await?;
        if user.mfa_backup_codes.len() < backup_before {
            warn!("🔐 '{}' signed in with a backup code; {} left.", user.login, user.mfa_backup_codes.len());
        }
    }
    Ok(true)
}

/// A user for a service (e.g. a plugin) with exactly these roles: created
/// with a random password nobody knows, or updated. Returns (ID, user).
pub async fn ensure_service_user(engine: &HexDBEngine, login: &str, roles: Vec<RoleGrant>) -> Result<(String, StoredUser)> {
    validate_login(login)?;
    check_roles_exist(engine, &roles).await?;
    if let Some((id, mut user)) = find_by_login(engine, login).await? {
        if user.roles != roles || user.is_locked {
            let _lock = engine.users_lock.lock().await;
            user.roles = roles;
            user.is_locked = false;
            engine.replace_document(USERS_TESSELLATION, &id.to_string(), to_json(&user)?, None, None).await?;
        }
        return Ok((id.to_string(), user));
    }
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let password = URL_SAFE_NO_PAD.encode(crate::crypt::random_bytes(24));
    let created = create_user(
        engine,
        NewUser { login: login.to_string(), password, email_address: format!("{}@service.hexdb.local", login), roles },
        None,
    )
    .await?;
    let (id, user) = find_by_id(engine, &created.value.id).await?.ok_or_else(|| anyhow!("the new user vanished"))?;
    Ok((id.to_string(), user))
}

/// True if `password` is the user's current password (to confirm sensitive actions).
pub async fn verify_password(engine: &HexDBEngine, user_id: &str, password: &str) -> Result<bool> {
    let Some((_, user)) = find_by_id(engine, user_id).await? else { return Ok(false) };
    Ok(crate::crypt::verify_hash(password, &user.password_hash))
}

/// Unused backup codes, for the Account page.
pub async fn mfa_status(engine: &HexDBEngine, user_id: &str) -> Result<(bool, usize, bool)> {
    let (_, user) = find_by_id(engine, user_id).await?.ok_or_else(|| not_found(user_id))?;
    Ok((user.use_mfa, user.mfa_backup_codes.len(), user.mfa_pending_secret.is_some()))
}

/// Find a user by ID, or by login (ignoring case).
async fn find_user(engine: &HexDBEngine, id_or_login: &str) -> Result<Option<(Ulid, StoredUser)>> {
    if let Ok(id) = Ulid::from_string(id_or_login) {
        if let Some(doc) = engine.get_document(USERS_TESSELLATION, &id.to_string()).await? {
            return Ok(parse_user(&doc).map(|u| (doc.id, u)));
        }
    }
    find_by_login(engine, id_or_login).await
}

async fn check_roles_exist(engine: &HexDBEngine, grants: &[RoleGrant]) -> Result<()> {
    let roles = list_roles(engine).await?;
    for grant in grants {
        if !roles.iter().any(|r| r.name == grant.name) {
            return Err(invalid(format!("Unknown role '{}'.", grant.name)));
        }
    }
    Ok(())
}

fn not_found(id_or_login: &str) -> anyhow::Error {
    EngineError::NotFound(format!("User '{}' not found.", id_or_login)).into()
}

fn to_json(user: &StoredUser) -> Result<Value> {
    let mut user = user.clone();
    user.login_key = login_key(&user.login);
    Ok(serde_json::to_value(user)?)
}

/// A user by login (ignoring case), through the login index.
async fn find_by_login(engine: &HexDBEngine, login: &str) -> Result<Option<(Ulid, StoredUser)>> {
    if !engine.tessellation_exists(USERS_TESSELLATION) {
        return Ok(None);
    }
    let filter = crate::filter::Filter::parse(&serde_json::json!({ "login_key": login_key(login) }))?;
    // Startup gives every user a login_key (see add_login_keys).
    let found = engine.matching_documents(USERS_TESSELLATION, &filter).await?;
    Ok(found.first().and_then(|doc| parse_user(doc).map(|u| (doc.id, u))))
}

/// All users.
pub async fn list_users(engine: &HexDBEngine) -> Result<Vec<UserView>> {
    Ok(all_users(engine).await?.iter().map(|(id, u)| UserView::new(*id, u)).collect())
}

/// One user, by ID or login.
pub async fn get_user(engine: &HexDBEngine, id_or_login: &str) -> Result<Option<UserView>> {
    Ok(find_user(engine, id_or_login).await?.map(|(id, u)| UserView::new(id, &u)))
}

/// Create a user.
pub async fn create_user(engine: &HexDBEngine, input: NewUser, idem: Option<IdempotencyKey>) -> Result<Outcome<UserView>> {
    let _lock = engine.users_lock.lock().await;
    if let Some(k) = &idem {
        if let Some(docs) = engine.replayed::<Vec<Document>>(k).await? {
            let doc = docs.into_iter().next().ok_or_else(|| anyhow!("Empty idempotent result"))?;
            let user = parse_user(&doc).ok_or_else(|| anyhow!("Stored user is unreadable"))?;
            return Ok(Outcome { value: UserView::new(doc.id, &user), replayed: true });
        }
    }

    validate_login(&input.login)?;
    validate_password_for(&input.password, &input.login)?;
    validate_email(&input.email_address)?;
    check_roles_exist(engine, &input.roles).await?;
    if find_user(engine, &input.login).await?.is_some() {
        return Err(EngineError::Conflict(format!("A user with login '{}' already exists.", input.login)).into());
    }

    let now = Utc::now().timestamp();
    let user = StoredUser {
        login: input.login,
        email_address: input.email_address,
        password_hash: create_hash(&input.password),
        roles: input.roles,
        created: now,
        last_login: 0,
        login_ips: Vec::new(),
        is_locked: false,
        password_attempts: 0,
        password_reset: false,
        password_reset_token: None,
        password_reset_expiry: 0,
        last_password_change: now,
        password_expiration: -1,
        use_mfa: false,
        mfa_secret: None,
        mfa_backup_codes: Vec::new(),
        mfa_pending_secret: None,
        mfa_last_step: 0,
        sessions_valid_after: 0,
        login_key: String::new(),
    };

    let outcome = engine.insert_documents(USERS_TESSELLATION, vec![to_json(&user)?], None, idem).await?;
    info!("👤 Created user '{}'.", user.login);
    Ok(outcome.map(|docs| UserView::new(docs[0].id, &user)))
}

/// Change a user. With `full_replace`, `email_address` and `roles` must be given.
pub async fn update_user(
    engine: &HexDBEngine,
    id_or_login: &str,
    changes: UserChanges,
    full_replace: bool,
    idem: Option<IdempotencyKey>,
) -> Result<Outcome<UserView>> {
    let _lock = engine.users_lock.lock().await;
    if let Some(k) = &idem {
        if let Some(doc) = engine.replayed::<Document>(k).await? {
            let user = parse_user(&doc).ok_or_else(|| anyhow!("Stored user is unreadable"))?;
            return Ok(Outcome { value: UserView::new(doc.id, &user), replayed: true });
        }
    }

    if full_replace && (changes.email_address.is_none() || changes.roles.is_none()) {
        return Err(invalid("Replacing a user requires email_address and roles."));
    }
    let (id, before) = find_user(engine, id_or_login).await?.ok_or_else(|| not_found(id_or_login))?;
    let mut user = before.clone();

    if let Some(login) = changes.login {
        validate_login(&login)?;
        if !login.eq_ignore_ascii_case(&user.login) && find_user(engine, &login).await?.is_some() {
            return Err(EngineError::Conflict(format!("A user with login '{}' already exists.", login)).into());
        }
        user.login = login;
    }
    if let Some(password) = changes.password {
        validate_password_for(&password, &user.login)?;
        user.password_hash = create_hash(&password);
        user.last_password_change = Utc::now().timestamp();
        user.password_attempts = 0;
        // A reset signs the user out everywhere.
        user.sessions_valid_after = Utc::now().timestamp_millis();
    }
    if let Some(email) = changes.email_address {
        validate_email(&email)?;
        user.email_address = email;
    }
    if let Some(roles) = changes.roles {
        check_roles_exist(engine, &roles).await?;
        user.roles = roles;
    }
    if let Some(locked) = changes.is_locked {
        if locked && !user.is_locked {
            user.sessions_valid_after = Utc::now().timestamp_millis();
        }
        user.is_locked = locked;
    }
    if let Some(use_mfa) = changes.use_mfa {
        if use_mfa && !user.use_mfa {
            return Err(invalid("Users turn MFA on themselves (Account page, or POST /auth/mfa/setup); administrators can only turn it off."));
        }
        if !use_mfa && user.use_mfa {
            // An administrator's reset, for a user who lost their authenticator and backup codes.
            user.use_mfa = false;
            user.mfa_secret = None;
            user.mfa_pending_secret = None;
            user.mfa_backup_codes.clear();
        }
    }
    if let Some(expiration) = changes.password_expiration {
        user.password_expiration = expiration;
    }

    if before.is_active_admin() && !user.is_active_admin() {
        ensure_another_admin(engine, id).await?;
    }

    let outcome = engine
        .replace_document(USERS_TESSELLATION, &id.to_string(), to_json(&user)?, None, idem)
        .await?;
    Ok(outcome.map(|doc| UserView::new(doc.id, &user)))
}

/// Delete a user. The value is false if the user doesn't exist.
pub async fn delete_user(engine: &HexDBEngine, id_or_login: &str, idem: Option<IdempotencyKey>) -> Result<Outcome<bool>> {
    let _lock = engine.users_lock.lock().await;
    if let Some(k) = &idem {
        if let Some(deleted) = engine.replayed::<bool>(k).await? {
            return Ok(Outcome { value: deleted, replayed: true });
        }
    }

    let Some((id, user)) = find_user(engine, id_or_login).await? else {
        return Ok(Outcome { value: false, replayed: false });
    };
    if user.is_active_admin() {
        ensure_another_admin(engine, id).await?;
    }
    let outcome = engine.delete_document(USERS_TESSELLATION, &id.to_string(), idem).await?;
    if outcome.value {
        info!("👤 Deleted user '{}'.", user.login);
    }
    Ok(outcome)
}

async fn ensure_another_admin(engine: &HexDBEngine, except: Ulid) -> Result<()> {
    let others = all_users(engine)
        .await?
        .iter()
        .filter(|(id, u)| *id != except && u.is_active_admin())
        .count();
    if others == 0 {
        return Err(EngineError::Conflict("This change would leave no unlocked administrator.".into()).into());
    }
    Ok(())
}

fn parse_role(doc: &Document) -> Option<RoleView> {
    let data = doc.data_json();
    let name = data.get("name")?.as_str()?.to_string();
    // Built-in roles always have their built-in permissions.
    let permissions = match builtin(&name) {
        Some(b) => b.permissions.to_vec(),
        None => data
            .get("permissions")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|p| p.as_str().and_then(Action::parse)).collect())
            .unwrap_or_default(),
    };
    Some(RoleView {
        id: doc.id.to_string(),
        builtin: builtin(&name).is_some(),
        name,
        description: data.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
        permissions,
    })
}

/// All roles.
pub async fn list_roles(engine: &HexDBEngine) -> Result<Vec<RoleView>> {
    if !engine.tessellation_exists(ROLES_TESSELLATION) {
        return Ok(Vec::new());
    }
    let page = engine.list_documents(ROLES_TESSELLATION, None, usize::MAX).await?;
    Ok(page.documents.iter().filter_map(parse_role).collect())
}

/// One role by name.
pub async fn get_role(engine: &HexDBEngine, name: &str) -> Result<Option<RoleView>> {
    Ok(list_roles(engine).await?.into_iter().find(|r| r.name == name))
}

/// Every role's permissions, for authorization. Cached until the next write
/// anywhere; the built-in roles are always present.
pub async fn role_definitions(engine: &HexDBEngine) -> Result<std::sync::Arc<RoleDefinitions>> {
    let generation = engine.generation();
    if let Some((cached_generation, definitions)) = engine.role_cache.lock().unwrap().as_ref() {
        if *cached_generation == generation {
            return Ok(definitions.clone());
        }
    }
    let mut definitions = RoleDefinitions::builtin();
    for role in list_roles(engine).await? {
        definitions.0.entry(role.name).or_insert(role.permissions);
    }
    let definitions = std::sync::Arc::new(definitions);
    *engine.role_cache.lock().unwrap() = Some((generation, definitions.clone()));
    Ok(definitions)
}

fn validate_role_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || "_-".contains(c)) {
        return Err(invalid("Role names are 1-64 letters, digits, '_' or '-'."));
    }
    Ok(())
}

fn parse_permissions(names: &[String]) -> Result<Vec<Action>> {
    let mut out = Vec::new();
    for name in names {
        let action = Action::parse(name.trim()).ok_or_else(|| {
            invalid(format!(
                "Unknown permission '{}'. Use: {}.",
                name,
                Action::ALL.iter().map(|a| a.name()).collect::<Vec<_>>().join(", ")
            ))
        })?;
        if !out.contains(&action) {
            out.push(action);
        }
    }
    out.sort();
    Ok(out)
}

fn role_json(name: &str, description: &str, permissions: &[Action]) -> Value {
    serde_json::json!({
        "name": name,
        "description": description,
        "permissions": permissions.iter().map(|a| a.name()).collect::<Vec<_>>(),
    })
}

/// Create a custom role.
pub async fn create_role(engine: &HexDBEngine, input: RoleInput) -> Result<RoleView> {
    let _lock = engine.users_lock.lock().await;
    let name = input.name.unwrap_or_default().trim().to_string();
    validate_role_name(&name)?;
    let permissions = parse_permissions(&input.permissions.unwrap_or_default())?;
    if permissions.is_empty() {
        return Err(invalid("A role needs at least one permission."));
    }
    if get_role(engine, &name).await?.is_some() || builtin(&name).is_some() {
        return Err(EngineError::Conflict(format!("A role named '{}' already exists.", name)).into());
    }
    let description = input.description.unwrap_or_default();
    if description.len() > 500 {
        return Err(invalid("description must be at most 500 characters."));
    }
    let docs = engine.insert_documents(ROLES_TESSELLATION, vec![role_json(&name, &description, &permissions)], None, None).await?;
    info!("🛡️ Created role '{}' ({}).", name, permissions.iter().map(|a| a.name()).collect::<Vec<_>>().join(", "));
    Ok(RoleView { id: docs.value[0].id.to_string(), name, description, permissions, builtin: false })
}

/// Change a custom role's description or permissions (built-in roles can't change).
pub async fn update_role(engine: &HexDBEngine, name: &str, input: RoleInput) -> Result<RoleView> {
    let _lock = engine.users_lock.lock().await;
    if builtin(name).is_some() {
        return Err(EngineError::Conflict(format!("'{}' is a built-in role and can't be changed. Create a custom role instead.", name)).into());
    }
    let role = get_role(engine, name).await?.ok_or_else(|| EngineError::NotFound(format!("Role '{}' not found.", name)))?;
    if input.name.as_deref().is_some_and(|n| n != name) {
        return Err(invalid("Roles can't be renamed; create a new role and move the grants."));
    }
    let permissions = match input.permissions {
        Some(p) => parse_permissions(&p)?,
        None => role.permissions.clone(),
    };
    if permissions.is_empty() {
        return Err(invalid("A role needs at least one permission."));
    }
    let description = input.description.unwrap_or(role.description);
    engine.replace_document(ROLES_TESSELLATION, &role.id, role_json(name, &description, &permissions), None, None).await?;
    info!("🛡️ Changed role '{}' ({}).", name, permissions.iter().map(|a| a.name()).collect::<Vec<_>>().join(", "));
    Ok(RoleView { id: role.id, name: name.to_string(), description, permissions, builtin: false })
}

/// Delete a custom role that no user holds. False if it doesn't exist.
pub async fn delete_role(engine: &HexDBEngine, name: &str) -> Result<bool> {
    let _lock = engine.users_lock.lock().await;
    if builtin(name).is_some() {
        return Err(EngineError::Conflict(format!("'{}' is a built-in role and can't be deleted.", name)).into());
    }
    let Some(role) = get_role(engine, name).await? else { return Ok(false) };
    let holders: Vec<String> = all_users(engine).await?.into_iter().filter(|(_, u)| u.roles.iter().any(|g| g.name == name)).map(|(_, u)| u.login).collect();
    if !holders.is_empty() {
        return Err(EngineError::Conflict(format!("Role '{}' is still granted to: {}. Remove it from them first.", name, holders.join(", "))).into());
    }
    engine.delete_document(ROLES_TESSELLATION, &role.id, None).await?;
    info!("🛡️ Deleted role '{}'.", name);
    Ok(true)
}

/// Create default roles and the configured admin user if missing, and
/// migrate users and roles stored in the old single-document format.
pub async fn bootstrap(engine: &HexDBEngine, security: &SecurityConfig) -> Result<()> {
    crate::engine::SKIP_ACKS.scope(true, bootstrap_inner(engine, security)).await
}

async fn bootstrap_inner(engine: &HexDBEngine, security: &SecurityConfig) -> Result<()> {
    // Replicas receive users and roles from the Overseer.
    if !engine.is_writable() {
        info!("🔐 This hex is a {}; users and roles are replicated from the Overseer.", engine.role());
        return Ok(());
    }
    let login = security.admin_login.trim();
    let configured_password = security.admin_password.trim();
    let email = security.admin_email.trim();
    if login.is_empty() || email.is_empty() {
        return Err(anyhow!("security.admin_login and security.admin_email must be set."));
    }

    engine.create_tessellation(ROLES_TESSELLATION, "system")?;
    engine.create_tessellation(USERS_TESSELLATION, "system")?;
    engine.prepare_audit().await?;
    migrate_legacy(engine).await?;
    add_login_keys(engine).await?;
    engine
        .ensure_internal_index(
            USERS_TESSELLATION,
            crate::index::IndexDef { name: LOGIN_INDEX.into(), kind: crate::index::IndexKind::Field, fields: vec!["login_key".into()], unique: false, analyzer: None },
        )
        .await?;

    // Built-in roles: add missing ones, and store their current description and permissions.
    let existing = list_roles(engine).await?;
    let page = engine.list_documents(ROLES_TESSELLATION, None, usize::MAX).await?;
    let mut missing = Vec::new();
    for role in BUILTIN_ROLES {
        let wanted = role_json(role.name, role.description, role.permissions);
        match existing.iter().find(|r| r.name == role.name) {
            None => missing.push(wanted),
            Some(found) => {
                let stored = page.documents.iter().find(|d| d.id.to_string() == found.id).map(|d| d.data_json());
                if stored.as_ref() != Some(&wanted) {
                    engine.replace_document(ROLES_TESSELLATION, &found.id, wanted, None, None).await?;
                }
            }
        }
    }
    if !missing.is_empty() {
        let count = missing.len();
        engine.insert_documents(ROLES_TESSELLATION, missing, None, None).await?;
        info!("✅ Added {} built-in role(s).", count);
    }

    if all_users(engine).await?.is_empty() {
        let generated = configured_password.is_empty();
        let password = if generated {
            use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
            URL_SAFE_NO_PAD.encode(crate::crypt::random_bytes(18))
        } else {
            validate_password_for(configured_password, login)
                .map_err(|e| anyhow!("security.admin_password can't be used for the first administrator: {}", e))?;
            configured_password.to_string()
        };
        let admin = NewUser {
            login: login.to_string(),
            password: password.clone(),
            email_address: email.to_string(),
            roles: vec![RoleGrant { name: ADMIN_ROLE.into(), tessellations: vec!["*".into()] }],
        };
        create_user(engine, admin, None).await?;
        if generated {
            // Shown once, on the console and in an owner-only file; never in the log buffer.
            let file = engine.config.storage_dir().join("initial-admin-password.txt");
            let note = format!(
                "HexDB created the first administrator.
  login:    {}
  password: {}
Sign in and change this password, then delete this file.
",
                login, password
            );
            let saved = crate::runtime::write_private_file(&file, note.as_bytes());
            eprintln!("
================================================================");
            eprintln!(" HexDB created the first administrator.");
            eprintln!("   login:    {}", login);
            eprintln!("   password: {}", password);
            eprintln!(" Sign in and change it. This is the only time it is shown.");
            eprintln!("================================================================
");
            match saved {
                Ok(()) => info!("✅ Admin user '{}' added with a generated password (printed to the console and saved to {}).", login, file.display()),
                Err(e) => warn!("⚠️ Admin user '{}' added with a generated password, printed to the console; saving it to {} failed: {:#}", login, file.display(), e),
            }
        } else {
            info!("✅ Admin user '{}' added with the configured password.", login);
        }
    } else if !configured_password.is_empty() {
        warn!("⚠️ security.admin_password is set but users already exist, so it is ignored. Remove it from the configuration.");
    }
    Ok(())
}

/// Give users stored before `login_key` existed their key.
async fn add_login_keys(engine: &HexDBEngine) -> Result<()> {
    let _lock = engine.users_lock.lock().await;
    for (id, user) in all_users(engine).await? {
        if user.login_key != login_key(&user.login) {
            engine.replace_document(USERS_TESSELLATION, &id.to_string(), to_json(&user)?, None, None).await?;
        }
    }
    Ok(())
}

/// Split legacy array documents into one document per user or role.
async fn migrate_legacy(engine: &HexDBEngine) -> Result<()> {
    for (tess, field) in [(USERS_TESSELLATION, "users"), (ROLES_TESSELLATION, "roles")] {
        let page = engine.list_documents(tess, None, usize::MAX).await?;
        for doc in page.documents {
            let data = doc.data_json();
            let Some(Value::Array(entries)) = data.get(field) else { continue };
            if data.get("login").is_some() || data.get("name").is_some() {
                continue;
            }

            let mut migrated = Vec::new();
            for entry in entries {
                let valid = match tess {
                    USERS_TESSELLATION => serde_json::from_value::<StoredUser>(entry.clone()).ok().map(|u| to_json(&u)).transpose()?,
                    _ => entry.get("name").and_then(Value::as_str).map(|_| entry.clone()),
                };
                match valid {
                    Some(json) => migrated.push(json),
                    None => warn!("⚠️ Skipping an unreadable legacy entry in '{}'.", tess),
                }
            }
            let count = migrated.len();
            if !migrated.is_empty() {
                engine.insert_documents(tess, migrated, None, None).await?;
            }
            engine.delete_document(tess, &doc.id.to_string(), None).await?;
            info!("🔁 Migrated {} legacy {} entr(ies) to one document each.", count, field);
        }
    }
    Ok(())
}
