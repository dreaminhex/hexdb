use anyhow::{anyhow, Result};
use argon2::{Argon2, PasswordHasher};
use argon2::password_hash::{PasswordHash, SaltString, rand_core::OsRng, PasswordVerifier};
use base64::{engine::general_purpose::STANDARD, Engine as _};

/// Length in bytes of the AES-256 storage encryption key.
pub const ENCRYPTION_KEY_LEN: usize = 32;

/// Hashes an input string using Argon2
/// Returns the hashed string.
pub fn create_hash(input: &str) -> String
{
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();

    let hashed = argon2
        .hash_password(input.as_bytes(), &salt)
        .expect("❌ Failed to hash password.")
        .to_string();

    return hashed;
}

/// Verifies an input against a hash using Argon2
/// Returns true if the input matches the hash, false otherwise.
pub fn verify_hash(input: &String, hash: &String) -> bool
{
    let parsed_hash = PasswordHash::new(hash).expect("❌ Failed to parse hash.");
    let argon2 = Argon2::default();

    match argon2.verify_password(input.as_bytes(), &parsed_hash) {
        Ok(_) => true,
        Err(_) => false,
    }
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
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"token", b"token"));
        assert!(!constant_time_eq(b"token", b"tokem"));
        assert!(!constant_time_eq(b"token", b""));
    }
}
