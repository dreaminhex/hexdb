// HexDB Core Multi-Factor Authentication
//
// Time-based one-time passwords (TOTP, RFC 6238: HMAC-SHA1, 30-second steps,
// 6 digits), the scheme every authenticator app supports, plus ten one-time
// backup codes for when the app is lost.
//
// Enrolment: `POST /auth/mfa/setup` (with the current password) creates a
// pending secret and returns it with an `otpauth://` URI for a QR code;
// `POST /auth/mfa/enable` with a valid code turns MFA on and returns the
// backup codes (shown once; only Argon2 hashes are stored). From then on,
// signing in with a password also needs `code`: a current TOTP code or an
// unused backup code. `POST /auth/mfa/disable` (password and code) turns it
// off; an administrator can reset it for a user who lost both.
//
// A TOTP code is accepted for the current step and one step either side (for
// clock drift), and never twice: the last used step is remembered per user.
// API keys are separate credentials and don't use MFA.

use crate::crypt::{create_hash, random_bytes, verify_hash};
use hmac::{Hmac, Mac};
use sha1::Sha1;

/// Seconds per TOTP step.
pub const STEP_SECONDS: i64 = 30;
/// Digits per code.
const DIGITS: u32 = 6;
/// Backup codes issued when MFA is enabled.
pub const BACKUP_CODES: usize = 10;

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32 without padding (how authenticator apps take secrets).
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for &b in bytes {
        buffer = (buffer << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(BASE32[((buffer >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(BASE32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for c in text.chars().filter(|c| !c.is_whitespace() && *c != '=' && *c != '-') {
        let value = BASE32.iter().position(|&b| b as char == c.to_ascii_uppercase())? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            out.push((buffer >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

/// A new random secret (160 bits, as RFC 4226 recommends), base32-encoded.
pub fn new_secret() -> String {
    base32_encode(&random_bytes(20))
}

/// The `otpauth://` URI authenticator apps scan.
pub fn otpauth_uri(issuer: &str, account: &str, secret: &str) -> String {
    fn encode(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
                _ => format!("%{:02X}", b),
            })
            .collect()
    }
    format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={}&period={}",
        encode(issuer),
        encode(account),
        secret,
        encode(issuer),
        DIGITS,
        STEP_SECONDS
    )
}

/// The code for a time step.
fn code_at(key: &[u8], step: i64) -> u32 {
    let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("HMAC takes keys of any length");
    mac.update(&(step as u64).to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let offset = (hash[19] & 0x0f) as usize;
    let binary = u32::from_be_bytes([hash[offset] & 0x7f, hash[offset + 1], hash[offset + 2], hash[offset + 3]]);
    binary % 10u32.pow(DIGITS)
}

/// The code for a base32 secret at a Unix time (for tools and tests).
pub fn code_for(secret: &str, unix_seconds: i64) -> Option<String> {
    let key = base32_decode(secret)?;
    Some(format!("{:06}", code_at(&key, unix_seconds.div_euclid(STEP_SECONDS))))
}

/// The time step a code matches (now, or one step either side), if any, and
/// only if it is later than `last_used_step`.
pub fn verify_totp(secret: &str, code: &str, now_seconds: i64, last_used_step: i64) -> Option<i64> {
    let code = code.trim().replace(' ', "");
    if code.len() != DIGITS as usize || !code.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let wanted: u32 = code.parse().ok()?;
    let key = base32_decode(secret)?;
    let now = now_seconds.div_euclid(STEP_SECONDS);
    // Check every candidate so timing doesn't reveal which step matched.
    let mut matched = None;
    for step in [now - 1, now, now + 1] {
        if crate::crypt::constant_time_eq(&code_at(&key, step).to_be_bytes(), &wanted.to_be_bytes()) && step > last_used_step {
            matched = Some(step);
        }
    }
    matched
}

/// New backup codes (`xxxxx-xxxxx`, 50 bits each) and their hashes.
pub fn new_backup_codes() -> (Vec<String>, Vec<String>) {
    let codes: Vec<String> = (0..BACKUP_CODES)
        .map(|_| {
            let raw = base32_encode(&random_bytes(7)).to_ascii_lowercase();
            format!("{}-{}", &raw[..5], &raw[5..10])
        })
        .collect();
    let hashes = codes.iter().map(|c| create_hash(c)).collect();
    (codes, hashes)
}

/// The index of the backup code hash `code` matches, if any.
pub fn match_backup_code(hashes: &[String], code: &str) -> Option<usize> {
    let code = code.trim().to_ascii_lowercase();
    if code.len() != 11 {
        return None;
    }
    hashes.iter().position(|h| verify_hash(&code, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_round_trips() {
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_decode("MZXW6YTBOI").unwrap(), b"foobar");
        assert_eq!(base32_decode("mzxw 6ytb-oi").unwrap(), b"foobar");
        assert!(base32_decode("1189").is_none());
        let secret = new_secret();
        assert_eq!(secret.len(), 32);
        assert_eq!(base32_decode(&secret).unwrap().len(), 20);
    }

    #[test]
    fn rfc_6238_vectors() {
        // RFC 6238 appendix B, SHA-1 key "12345678901234567890", last 6 digits.
        let key = b"12345678901234567890";
        for (time, code) in [(59, 287082), (1111111109, 81804), (1111111111, 50471), (1234567890, 5924), (2000000000, 279037)] {
            assert_eq!(code_at(key, time / STEP_SECONDS), code, "t={}", time);
        }
    }

    #[test]
    fn codes_verify_within_one_step_and_never_twice() {
        let secret = base32_encode(b"12345678901234567890");
        assert_eq!(verify_totp(&secret, "287082", 59, 0), Some(1));
        assert_eq!(verify_totp(&secret, "287 082", 59 + 30, 0), Some(1), "one step late is fine");
        assert_eq!(verify_totp(&secret, "287082", 59 + 90, 0), None, "three steps late is not");
        assert_eq!(verify_totp(&secret, "287082", 59, 1), None, "a used step is refused");
        assert_eq!(verify_totp(&secret, "000000", 59, 0), None);
        assert_eq!(verify_totp(&secret, "28708", 59, 0), None);
        assert_eq!(verify_totp(&secret, "abcdef", 59, 0), None);
    }

    #[test]
    fn backup_codes_match_once_each() {
        let (codes, hashes) = new_backup_codes();
        assert_eq!(codes.len(), BACKUP_CODES);
        assert!(codes.iter().all(|c| c.len() == 11 && c.as_bytes()[5] == b'-'));
        assert_eq!(match_backup_code(&hashes, &codes[3].to_uppercase()), Some(3));
        assert_eq!(match_backup_code(&hashes, "aaaaa-bbbbb"), None);
    }

    #[test]
    fn otpauth_uri_is_escaped() {
        let uri = otpauth_uri("HexDB Nebula", "ada@example.com", "ABC");
        assert_eq!(uri, "otpauth://totp/HexDB%20Nebula:ada%40example.com?secret=ABC&issuer=HexDB%20Nebula&algorithm=SHA1&digits=6&period=30");
    }
}
