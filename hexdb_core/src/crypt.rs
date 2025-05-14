use argon2::{Argon2, PasswordHasher};
use argon2::password_hash::{PasswordHash, SaltString, rand_core::OsRng, PasswordVerifier};

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
