// HexDB Core Users and Roles
//
// Users and roles are stored as documents in the `users` and `roles` system
// tessellations, one document per user or role. This module validates input,
// hashes passwords with Argon2, keeps logins unique (ignoring case), protects
// the last administrator, and never returns password hashes or MFA secrets.
//
// Earlier builds stored all users in one document holding a `users` array and
// all roles in one document holding a `roles` array; `bootstrap` migrates those.

use crate::{
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

const DEFAULT_ROLES: &[(&str, &str)] = &[
    ("admin", "Full system access."),
    ("reader", "Read-only access to specified tessellations."),
    ("writer", "Write access to specified tessellations (assumes read access)."),
    ("owner", "Full access to specified tessellations."),
];
const MIN_PASSWORD_LEN: usize = 12;
const MAX_PASSWORD_LEN: usize = 1024;

/// A role granted to a user, with the tessellations it applies to ("*" for all).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleGrant {
    pub name: String,
    #[serde(default)]
    pub permissions: Vec<String>,
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
    #[serde(default)]
    pub mfa_backup_codes: Vec<String>,
    /// Sessions issued at or before this time (epoch milliseconds) are no longer valid.
    #[serde(default)]
    pub sessions_valid_after: i64,
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
    if !engine.tessellation_exists(USERS_TESSELLATION) {
        return Ok(None);
    }
    Ok(all_users(engine).await?.into_iter().find(|(_, u)| u.login.eq_ignore_ascii_case(login.trim())))
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

/// Find a user by ID, or by login (ignoring case).
async fn find_user(engine: &HexDBEngine, id_or_login: &str) -> Result<Option<(Ulid, StoredUser)>> {
    if let Ok(id) = Ulid::from_string(id_or_login) {
        if let Some(doc) = engine.get_document(USERS_TESSELLATION, &id.to_string()).await? {
            return Ok(parse_user(&doc).map(|u| (doc.id, u)));
        }
    }
    Ok(all_users(engine)
        .await?
        .into_iter()
        .find(|(_, u)| u.login.eq_ignore_ascii_case(id_or_login)))
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
    Ok(serde_json::to_value(user)?)
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
        sessions_valid_after: 0,
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
        user.use_mfa = use_mfa;
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

/// All roles.
pub async fn list_roles(engine: &HexDBEngine) -> Result<Vec<RoleView>> {
    let page = engine.list_documents(ROLES_TESSELLATION, None, usize::MAX).await?;
    Ok(page
        .documents
        .iter()
        .filter_map(|doc| {
            let data = doc.data_json();
            Some(RoleView {
                id: doc.id.to_string(),
                name: data.get("name")?.as_str()?.to_string(),
                description: data.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
            })
        })
        .collect())
}

/// One role by name.
pub async fn get_role(engine: &HexDBEngine, name: &str) -> Result<Option<RoleView>> {
    Ok(list_roles(engine).await?.into_iter().find(|r| r.name == name))
}

/// Create default roles and the configured admin user if missing, and
/// migrate users and roles stored in the old single-document format.
pub async fn bootstrap(engine: &HexDBEngine, security: &SecurityConfig) -> Result<()> {
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
    migrate_legacy(engine).await?;

    let existing: Vec<String> = list_roles(engine).await?.into_iter().map(|r| r.name).collect();
    let missing: Vec<Value> = DEFAULT_ROLES
        .iter()
        .filter(|(name, _)| !existing.iter().any(|e| e == name))
        .map(|(name, description)| serde_json::json!({ "name": name, "description": description }))
        .collect();
    if !missing.is_empty() {
        let count = missing.len();
        engine.insert_documents(ROLES_TESSELLATION, missing, None, None).await?;
        info!("✅ Added {} default role(s).", count);
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
            roles: vec![RoleGrant { name: ADMIN_ROLE.into(), permissions: vec!["*".into()] }],
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
