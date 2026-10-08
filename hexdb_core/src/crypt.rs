// HexDB Core cryptography: password hashing, the storage key ring, and
// constant-time comparison.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{anyhow, Result};
use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordVerifier, SaltString};
use argon2::{Argon2, PasswordHasher};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rand::RngCore;

/// Length in bytes of the AES-256 storage encryption key.
pub const ENCRYPTION_KEY_LEN: usize = 32;
/// Length of an AES-GCM nonce.
pub const NONCE_LEN: usize = 12;

/// Hash a password with Argon2id and a random salt (PHC string format).
pub fn create_hash(input: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(input.as_bytes(), &salt)
        .map(|h| h.to_string())
        // Hashing only fails for out-of-range parameters, which the defaults aren't.
        .unwrap_or_default()
}

/// True if `input` matches an Argon2 hash. A malformed or empty hash never matches.
pub fn verify_hash(input: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default().verify_password(input.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}

/// `count` random bytes from the operating system's CSPRNG.
pub fn random_bytes(count: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; count];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

/// Encryption keys for data at rest: the current key, used for every new
/// write, and previous keys, still accepted for reading so a key can be
/// rotated without downtime. Each key has a stable 64-bit ID (derived from the
/// key, revealing nothing about it) that SSTables record.
#[derive(Clone)]
pub struct KeyRing {
    current: (u64, Aes256Gcm),
    previous: Vec<(u64, Aes256Gcm)>,
}

impl std::fmt::Debug for KeyRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyRing")
            .field("current", &format_args!("{:016x}", self.current.0))
            .field("previous", &self.previous.iter().map(|(id, _)| format!("{:016x}", id)).collect::<Vec<_>>())
            .finish()
    }
}

impl KeyRing {
    pub fn new(current: &[u8; ENCRYPTION_KEY_LEN], previous: &[[u8; ENCRYPTION_KEY_LEN]]) -> KeyRing {
        let entry = |key: &[u8; ENCRYPTION_KEY_LEN]| (Self::key_id(key), Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)));
        KeyRing {
            current: entry(current),
            previous: previous.iter().filter(|k| *k != current).map(entry).collect(),
        }
    }

    /// A key's public identifier.
    pub fn key_id(key: &[u8; ENCRYPTION_KEY_LEN]) -> u64 {
        let derived = blake3::derive_key("HexDB 2026 storage key id v1", key);
        u64::from_be_bytes(derived[..8].try_into().unwrap())
    }

    pub fn current_id(&self) -> u64 {
        self.current.0
    }

    pub fn has_key(&self, id: u64) -> bool {
        self.current.0 == id || self.previous.iter().any(|(k, _)| *k == id)
    }

    /// The current key's cipher (for the WAL writer).
    pub fn current_cipher(&self) -> &Aes256Gcm {
        &self.current.1
    }

    /// Encrypt with the current key: `nonce || ciphertext`. `aad` is
    /// authenticated but not stored; decryption needs the same value.
    pub fn encrypt(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce);
        let ciphertext = self
            .current
            .1
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad })
            .map_err(|_| anyhow!("encryption failed"))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypt `nonce || ciphertext` with the key that has this ID.
    pub fn decrypt(&self, key_id: u64, data: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let cipher = std::iter::once(&self.current)
            .chain(self.previous.iter())
            .find(|(id, _)| *id == key_id)
            .map(|(_, c)| c)
            .ok_or_else(|| anyhow!("no configured key has ID {:016x}", key_id))?;
        decrypt_with(cipher, data, aad)
    }

    /// Decrypt with whichever configured key works (for records that don't
    /// name their key, like WAL records).
    pub fn decrypt_any(&self, data: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        std::iter::once(&self.current)
            .chain(self.previous.iter())
            .find_map(|(_, cipher)| decrypt_with(cipher, data, aad).ok())
            .ok_or_else(|| anyhow!("decryption failed (no configured key works, or the data is corrupt)"))
    }
}

fn decrypt_with(cipher: &Aes256Gcm, data: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    if data.len() < NONCE_LEN {
        return Err(anyhow!("ciphertext too short"));
    }
    let (nonce, ciphertext) = data.split_at(NONCE_LEN);
    cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ciphertext, aad })
        .map_err(|_| anyhow!("decryption failed (wrong key or corrupt data)"))
}

/// Decodes the configured storage encryption key.
/// The value must look like `base64:<base64 data>` and decode to exactly 32 bytes (AES-256).
pub fn decode_encryption_key(value: &str) -> Result<[u8; ENCRYPTION_KEY_LEN]> {
    const HINT: &str = "Generate one with `openssl rand -base64 32` and set storage.encryption_key = \"base64:<value>\".";

    let value = value.trim();
    if value.is_empty() {
        return Err(anyhow!("storage.encryption_key is not set. {}", HINT));
    }

    let encoded = value
        .strip_prefix("base64:")
        .ok_or_else(|| anyhow!("storage.encryption_key must start with \"base64:\". {}", HINT))?;

    let bytes = STANDARD
        .decode(encoded.trim())
        .map_err(|e| anyhow!("storage.encryption_key is not valid base64 ({}). {}", e, HINT))?;

    let len = bytes.len();
    bytes.try_into().map_err(|_| {
        anyhow!(
            "storage.encryption_key must decode to {} bytes for AES-256, but it decodes to {} bytes. {}",
            ENCRYPTION_KEY_LEN,
            len,
            HINT
        )
    })
}

/// Compares two byte strings without leaking where they differ through timing.
/// Both inputs are hashed with BLAKE3, whose hash equality check is constant-time.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    blake3::hash(a) == blake3::hash(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_valid_key() {
        let key = format!("base64:{}", STANDARD.encode([7u8; 32]));
        assert_eq!(decode_encryption_key(&key).unwrap(), [7u8; 32]);
    }

    #[test]
    fn rejects_bad_keys() {
        assert!(decode_encryption_key("").is_err());
        assert!(decode_encryption_key("base64:...").is_err());
        assert!(decode_encryption_key(&STANDARD.encode([7u8; 32])).is_err()); // missing prefix
        let short = format!("base64:{}", STANDARD.encode([7u8; 16]));
        let err = decode_encryption_key(&short).unwrap_err().to_string();
        assert!(err.contains("16 bytes"), "{}", err);
    }

    #[test]
    fn verify_hash_never_panics() {
        let hash = create_hash("correct horse battery");
        assert!(verify_hash("correct horse battery", &hash));
        assert!(!verify_hash("wrong", &hash));
        assert!(!verify_hash("anything", ""));
        assert!(!verify_hash("anything", "not a hash"));
        assert!(!verify_hash("anything", "$argon2id$v=19$m=bad"));
    }

    #[test]
    fn key_ring_encrypts_rotates_and_binds_aad() {
        let (old, new) = ([1u8; 32], [2u8; 32]);
        let before = KeyRing::new(&old, &[]);
        let sealed = before.encrypt(b"secret", b"doc-1").unwrap();
        assert_ne!(&sealed[NONCE_LEN..], b"secret");
        assert_eq!(before.decrypt(before.current_id(), &sealed, b"doc-1").unwrap(), b"secret");
        assert!(before.decrypt(before.current_id(), &sealed, b"doc-2").is_err(), "AAD is bound");

        let after = KeyRing::new(&new, &[old]);
        assert_ne!(after.current_id(), before.current_id());
        assert!(after.has_key(before.current_id()));
        assert_eq!(after.decrypt(before.current_id(), &sealed, b"doc-1").unwrap(), b"secret");
        assert_eq!(after.decrypt_any(&sealed, b"doc-1").unwrap(), b"secret");

        let stranger = KeyRing::new(&[3u8; 32], &[]);
        assert!(stranger.decrypt_any(&sealed, b"doc-1").is_err());
        assert!(!format!("{:?}", after).contains("Aes"), "debug output names key IDs only");
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"token", b"token"));
        assert!(!constant_time_eq(b"token", b"tokem"));
        assert!(!constant_time_eq(b"token", b""));
    }
}
