// HexDB Core Sharing Links
//
// A sharing link gives people without an account one file, or the files in
// one folder, of a files tessellation: `https://<hex>/_s/<code>`.
//
// Codes use MAR-62 (from gotoit.us, by this project's author): an
// HMAC-SHA256 over `timestamp | nonce | what is linked`, whose first 47 bits
// are written in Base62 (0-9, A-Z, a-z) and padded to 8 characters. The HMAC
// key is derived from the lattice key, so codes can't be predicted without
// it. A code maps to its link's document ID in `_shares` (the 47-bit value),
// so looking one up is a direct read; a code already in use is drawn again
// with a new nonce.
//
// A link is a bearer credential, so:
//   * it reaches only what it names: one file, or the files under one folder;
//   * it acts as the user who made it, resolved on every use: their read
//     permission and row filters apply, and a link stops working when they
//     lose access, are locked or are removed;
//   * it can expire (`sharing.max_days`), need a password (Argon2; unlocking
//     gives a short-lived token for the downloads), and allow a number of
//     downloads; its creator or an administrator can delete it;
//   * unknown, expired and deleted codes and wrong passwords all look the
//     same, and count toward a per-address lockout (the sign-in limits), so
//     codes can't be guessed online.

use crate::{
    auth::{Action, Credential, Permission, Principal},
    engine::{EngineError, HexDBEngine},
};
use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use std::time::Duration;
use ulid::Ulid;

/// Sharing links, one document per link.
pub const SHARES_TESSELLATION: &str = "_shares";
const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
/// Characters in a code.
pub const CODE_LEN: usize = 8;
/// How long an unlocked password-protected link stays unlocked.
pub const UNLOCK_MINUTES: i64 = 60;
const MAX_NOTE: usize = 500;

// ---------------------------------------------------------------------------
// MAR-62
// ---------------------------------------------------------------------------

/// `value` in Base62.
pub fn base62_encode(mut value: u64) -> String {
    if value == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while value > 0 {
        out.push(ALPHABET[(value % 62) as usize]);
        value /= 62;
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII")
}

/// The value of a Base62 string, if it is one (and fits in 64 bits).
pub fn base62_decode(text: &str) -> Option<u64> {
    text.bytes().try_fold(0u64, |value, c| {
        let digit = ALPHABET.iter().position(|a| *a == c)? as u64;
        value.checked_mul(62)?.checked_add(digit)
    })
}

/// A MAR-62 code: HMAC-SHA256 under `key` of `timestamp|nonce|parts...`, the
/// first 47 bits in Base62, padded with zeros to 8 characters.
pub fn mar62(key: &[u8], timestamp_nanos: i128, nonce: &str, parts: &[&str]) -> String {
    let input = format!("{}|{}|{}", timestamp_nanos, nonce, parts.join("|"));
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(input.as_bytes());
    let hash = mac.finalize().into_bytes();
    let value = hash[..6].iter().fold(0u64, |v, b| (v << 8) | *b as u64) & 0x7FFF_FFFF_FFFF;
    format!("{:0>8}", base62_encode(value))
}

/// The document ID of a code's link, if `code` is a well-formed code.
pub fn code_id(code: &str) -> Option<Ulid> {
    if code.len() != CODE_LEN || !code.bytes().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let value = base62_decode(code)?;
    (value <= 0x7FFF_FFFF_FFFF).then(|| Ulid::from(value as u128))
}

fn code_key(engine: &HexDBEngine) -> Result<[u8; 32]> {
    Ok(blake3::derive_key("HexDB 2026 sharing link codes v1", &engine.config.lattice_key()?))
}

fn token_keys(engine: &HexDBEngine) -> Result<Vec<[u8; 32]>> {
    Ok(engine.config.lattice_keys()?.iter().map(|k| blake3::derive_key("HexDB 2026 sharing link unlock v1", k)).collect())
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

/// A sharing link as stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Share {
    pub code: String,
    pub tessellation: String,
    /// A file's ID, or none for a folder link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    /// A folder (`/a/b/`), for a folder link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// The file's name, or the folder's, when the link was made.
    pub name: String,
    pub created_by_id: String,
    pub created_by: String,
    pub created_at: i64,
    /// Epoch milliseconds; none: no expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_downloads: Option<u64>,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub note: String,
}

impl Share {
    pub fn expired(&self) -> bool {
        self.expires_at.is_some_and(|t| t <= Utc::now().timestamp_millis()) || self.max_downloads.is_some_and(|m| self.downloads >= m)
    }

    /// The link for its owner and administrators (never the password hash).
    pub fn view(&self) -> Value {
        json!({
            "code": self.code,
            "path": format!("/_s/{}", self.code),
            "tessellation": self.tessellation,
            "file_id": self.file_id,
            "folder": self.folder,
            "name": self.name,
            "created_by": self.created_by,
            "created_at": self.created_at,
            "expires_at": self.expires_at,
            "password": self.password_hash.is_some(),
            "max_downloads": self.max_downloads,
            "downloads": self.downloads,
            "note": self.note,
            "expired": self.expired(),
        })
    }

    /// What anyone with the code may know before unlocking it.
    pub fn public_view(&self) -> Value {
        json!({
            "code": self.code,
            "kind": if self.file_id.is_some() { "file" } else { "folder" },
            "name": self.name,
            "expires_at": self.expires_at,
            "password": self.password_hash.is_some(),
            "downloads_left": self.max_downloads.map(|m| m.saturating_sub(self.downloads)),
        })
    }
}

/// What a user asks for when making a link.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewShare {
    pub tessellation: String,
    #[serde(default)]
    pub file_id: Option<String>,
    #[serde(default)]
    pub folder: Option<String>,
    /// Days until it expires (fractions allowed); none: `sharing.default_days`.
    #[serde(default)]
    pub expires_in_days: Option<f64>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub max_downloads: Option<u64>,
    #[serde(default)]
    pub note: Option<String>,
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    EngineError::Invalid(message.into()).into()
}

impl HexDBEngine {
    fn sharing_number(&self, key: &str, default: f64) -> f64 {
        self.config_value(key).as_f64().unwrap_or(default)
    }

    /// Make a link (on the Overseer) to something `principal` can read.
    pub async fn create_share(&self, principal: &Principal, input: NewShare) -> Result<Share> {
        self.ensure_writable()?;
        if self.config_value("sharing.enabled") == json!(false) {
            return Err(EngineError::Forbidden("Sharing links are turned off (sharing.enabled).".into()).into());
        }
        principal.require_action(Action::Share)?;
        principal.require(Permission::Read, &input.tessellation)?;
        self.files_tessellation(&input.tessellation)?;
        let (file_id, folder, name) = match (input.file_id.as_deref().map(str::trim).filter(|s| !s.is_empty()), input.folder.as_deref()) {
            (Some(id), None) => {
                // Visible to its creator now (row filters apply; see crate::access).
                let (doc, _) = crate::access::as_caller(std::sync::Arc::new(principal.clone()), self.visible_file(&input.tessellation, id)).await?;
                let name = doc.data_json()["name"].as_str().unwrap_or_default().to_string();
                (Some(doc.id.to_string()), None, name)
            }
            (None, Some(path)) => {
                let folder = crate::files::normalize_path(path)?;
                let name = folder.trim_end_matches('/').rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("/").to_string();
                (None, Some(folder), name)
            }
            _ => return Err(invalid("Give either file_id or folder.")),
        };
        let max_days = self.sharing_number("sharing.max_days", 30.0);
        let days = input.expires_in_days.unwrap_or_else(|| self.sharing_number("sharing.default_days", 7.0));
        if days < 0.0 || (max_days > 0.0 && (days == 0.0 || days > max_days)) {
            return Err(invalid(if max_days > 0.0 { format!("Links expire within {} days here.", max_days) } else { "expires_in_days can't be negative.".to_string() }));
        }
        let expires_at = (days > 0.0).then(|| Utc::now().timestamp_millis() + (days * 86_400_000.0) as i64);
        let password = input.password.as_deref().filter(|p| !p.is_empty());
        if self.config_value("sharing.require_password") == json!(true) && password.is_none() {
            return Err(invalid("Links here need a password (sharing.require_password)."));
        }
        if let Some(p) = password {
            if p.chars().count() < 8 || p.len() > 1024 {
                return Err(invalid("A link's password is 8-1024 characters."));
            }
        }
        if input.max_downloads == Some(0) {
            return Err(invalid("max_downloads must be at least 1 (or left out)."));
        }
        let note = input.note.unwrap_or_default().trim().to_string();
        if note.chars().count() > MAX_NOTE {
            return Err(invalid(format!("Notes are at most {} characters.", MAX_NOTE)));
        }

        let key = code_key(self)?;
        let target = file_id.clone().or_else(|| folder.clone()).unwrap_or_default();
        for _ in 0..8 {
            let nonce = hex::encode(crate::crypt::random_bytes(3));
            let code = mar62(&key, Utc::now().timestamp_nanos_opt().unwrap_or_default() as i128, &nonce, &[&input.tessellation, &target]);
            let id = code_id(&code).ok_or_else(|| anyhow!("MAR-62 produced an invalid code"))?;
            if self.get_system_document(SHARES_TESSELLATION, id).await?.is_some() {
                continue;
            }
            let share = Share {
                code,
                tessellation: input.tessellation.clone(),
                file_id: file_id.clone(),
                folder: folder.clone(),
                name: name.clone(),
                created_by_id: principal.user_id.clone(),
                created_by: principal.login.clone(),
                created_at: Utc::now().timestamp_millis(),
                expires_at,
                password_hash: password.map(crate::crypt::create_hash),
                max_downloads: input.max_downloads,
                downloads: 0,
                note: note.clone(),
            };
            self.put_system_document(SHARES_TESSELLATION, id, serde_json::to_value(&share)?, None).await?;
            return Ok(share);
        }
        Err(anyhow!("couldn't find a free link code; try again"))
    }

    /// A link by its code, whatever its state (none: no such code).
    pub async fn share(&self, code: &str) -> Result<Option<Share>> {
        let Some(id) = code_id(code) else { return Ok(None) };
        let Some(doc) = self.get_system_document(SHARES_TESSELLATION, id).await? else { return Ok(None) };
        Ok(serde_json::from_value::<Share>(doc.data_json()).ok().filter(|s| s.code == code))
    }

    /// Links made by `principal` (every link, for an administrator with `all`).
    pub async fn list_shares(&self, principal: &Principal, all: bool) -> Result<Vec<Share>> {
        if !self.tessellation_exists(SHARES_TESSELLATION) {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = self.list_documents(SHARES_TESSELLATION, after, 1000).await?;
            for doc in &page.documents {
                if let Ok(share) = serde_json::from_value::<Share>(doc.data_json()) {
                    if (all && principal.is_admin()) || share.created_by_id == principal.user_id {
                        out.push(share);
                    }
                }
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        Ok(out)
    }

    /// Delete a link (its creator or an administrator).
    pub async fn delete_share(&self, principal: &Principal, code: &str) -> Result<Share> {
        self.ensure_writable()?;
        let share = self.share(code).await?.filter(|s| s.created_by_id == principal.user_id || principal.is_admin());
        let share = share.ok_or_else(|| EngineError::NotFound(format!("No link {}.", code)))?;
        self.delete_system_document(SHARES_TESSELLATION, code_id(code).expect("a valid code")).await?;
        Ok(share)
    }

    /// Count one download (on the Overseer; a replica sends it there).
    pub async fn count_share_download(&self, code: &str) {
        let outcome = if self.is_writable() {
            self.store_share_download(code).await
        } else {
            tokio::time::timeout(Duration::from_secs(3), crate::replication::post_to_overseer(self, "/lattice/share-download", &json!({ "code": code })))
                .await
                .unwrap_or_else(|_| Err(anyhow!("timed out")))
        };
        if let Err(e) = outcome {
            tracing::warn!("⚠️ Couldn't count a download of link {}: {:#}", code, e);
        }
    }

    pub async fn store_share_download(&self, code: &str) -> Result<()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _guard = LOCK.lock().await;
        let Some(mut share) = self.share(code).await? else { return Ok(()) };
        share.downloads += 1;
        self.put_system_document(SHARES_TESSELLATION, code_id(code).expect("a valid code"), serde_json::to_value(&share)?, None).await
    }

    /// The link's creator as a principal, resolved now: none if they're gone,
    /// locked, or can no longer read the link's tessellation.
    pub async fn share_principal(&self, share: &Share) -> Result<Option<Principal>> {
        let Some((id, user)) = crate::users::find_by_id(self, &share.created_by_id).await? else { return Ok(None) };
        if user.is_locked {
            return Ok(None);
        }
        let definitions = crate::users::role_definitions(self).await?;
        let principal = Principal::new(
            id.to_string(),
            user.login,
            user.email_address,
            user.roles,
            Credential::Session { session_id: format!("share:{}", share.code), expires_at: share.expires_at.unwrap_or(0) / 1000 },
            &definitions,
        )
        .with_attributes(user.attributes);
        Ok((principal.has(Action::Share) && principal.can(Permission::Read, &share.tessellation)).then_some(principal))
    }

    /// A token that unlocks a password-protected link for an hour.
    pub fn share_unlock_token(&self, code: &str) -> Result<String> {
        let claims = json!({ "c": code, "e": Utc::now().timestamp() + UNLOCK_MINUTES * 60 }).to_string();
        let key = token_keys(self)?.into_iter().next().ok_or_else(|| anyhow!("no lattice key"))?;
        let mac = blake3::keyed_hash(&key, claims.as_bytes());
        Ok(format!("{}.{}", URL_SAFE_NO_PAD.encode(claims), URL_SAFE_NO_PAD.encode(mac.as_bytes())))
    }

    /// True if `token` unlocks `code` now.
    pub fn share_token_valid(&self, code: &str, token: &str) -> bool {
        let Some((claims, mac)) = token.split_once('.') else { return false };
        let (Ok(claims), Ok(mac)) = (URL_SAFE_NO_PAD.decode(claims), URL_SAFE_NO_PAD.decode(mac)) else { return false };
        let Ok(mac): Result<[u8; 32], _> = mac.try_into() else { return false };
        let signed = token_keys(self).unwrap_or_default().iter().any(|k| blake3::keyed_hash(k, &claims) == blake3::Hash::from(mac));
        let Ok(claims) = serde_json::from_slice::<Value>(&claims) else { return false };
        signed && claims["c"] == code && claims["e"].as_i64().is_some_and(|e| e > Utc::now().timestamp())
    }

    /// Refuse while this address has made too many bad link attempts (the sign-in limits).
    pub async fn share_attempt_allowed(&self, client: &str) -> Result<()> {
        let key = format!("share:{}", client);
        let blocked = match self.login_throttle.blocked(&[&key]) {
            Some(wait) => Some(wait),
            None => crate::auth::lattice_blocked(self, &[&key]).await,
        };
        match blocked {
            Some(wait) => Err(EngineError::RateLimited("Too many attempts. Try again later.".into(), wait.as_secs().max(1)).into()),
            None => Ok(()),
        }
    }

    /// Count a bad link attempt (unknown code, wrong password) from this address.
    pub async fn share_attempt_failed(&self, client: &str) {
        let key = format!("share:{}", client);
        self.login_throttle.fail(&[&key]);
        crate::auth::share_throttle(self, &[&key], &[]).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base62_round_trips() {
        for value in [0u64, 1, 61, 62, 3843, 0x7FFF_FFFF_FFFF, u64::MAX] {
            assert_eq!(base62_decode(&base62_encode(value)), Some(value));
        }
        assert_eq!(base62_encode(61), "z");
        assert_eq!(base62_encode(62), "10");
        assert_eq!(base62_decode("not-base62"), None);
    }

    #[test]
    fn mar62_codes_are_eight_characters_and_keyed() {
        let a = mar62(b"key one", 1_700_000_000_000_000_000, "a1b2c3", &["media", "01JABC"]);
        let b = mar62(b"key two", 1_700_000_000_000_000_000, "a1b2c3", &["media", "01JABC"]);
        let c = mar62(b"key one", 1_700_000_000_000_000_001, "a1b2c3", &["media", "01JABC"]);
        assert_eq!(a.len(), CODE_LEN);
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, b, "another key, another code");
        assert_ne!(a, c, "another time, another code");
        assert_eq!(a, mar62(b"key one", 1_700_000_000_000_000_000, "a1b2c3", &["media", "01JABC"]), "deterministic");
        // 47 bits always fit in 8 Base62 characters (62^8 > 2^47).
        assert!(base62_encode(0x7FFF_FFFF_FFFF).len() <= CODE_LEN);
    }

    #[test]
    fn codes_map_to_ids_and_bad_codes_dont() {
        let code = mar62(b"k", 1, "n", &["t"]);
        let id = code_id(&code).unwrap();
        assert_eq!(format!("{:0>8}", base62_encode(u128::from(id) as u64)), code);
        for bad in ["", "short", "toolong123", "bad-code", "zzzzzzzz"] {
            assert!(code_id(bad).is_none(), "{}", bad);
        }
    }
}
