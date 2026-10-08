// HexDB lattice authentication
//
// Hexes prove to each other that they hold the lattice key
// (`HexConfig::lattice_key`) without ever sending it:
//
//   Discovery (TCP):
//     prober    -> HEXDB_HELLO2 <unix seconds> <nonce> <mac("hello", ts, nonce)>
//     responder -> HEXDB_IDENTITY <json> <mac("identity", nonce, json)>
//   A responder answers only a valid, fresh (±60 s), never-seen hello, so
//   strangers learn nothing; the prober accepts only an identity bound to its
//   own nonce, so a replayed or forged reply can't impersonate an Overseer.
//
//   Replication (HTTP): every request to /lattice/* carries
//     X-HexDB-Lattice-Signature: <unix seconds>.<nonce>.<mac("request", ts, nonce, METHOD, path?query, body hash)>
//   checked for freshness (±60 s) and single use.
//
// MACs are keyed BLAKE3 with keys derived from the lattice key, compared in
// constant time.

use crate::crypt::random_bytes;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const LATTICE_SIGNATURE_HEADER: &str = "x-hexdb-lattice-signature";
const MAX_SKEW_SECONDS: i64 = 60;

fn mac(key: &[u8; 32], parts: &[&[u8]]) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new_keyed(key);
    for part in parts {
        // Length-prefix each part so boundaries can't be shifted.
        hasher.update(&(part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize()
}

fn parse_mac(hex: &str) -> Option<blake3::Hash> {
    blake3::Hash::from_hex(hex).ok()
}

fn fresh(ts: i64) -> bool {
    (chrono::Utc::now().timestamp() - ts).abs() <= MAX_SKEW_SECONDS
}

fn new_nonce() -> String {
    hex::encode(random_bytes(16))
}

/// Single-use nonces seen within the freshness window.
#[derive(Default)]
pub struct NonceCache {
    seen: Mutex<HashMap<String, Instant>>,
}

impl NonceCache {
    /// True the first time a nonce is seen (within the window).
    pub fn first_use(&self, nonce: &str) -> bool {
        let now = Instant::now();
        let window = Duration::from_secs((MAX_SKEW_SECONDS * 2) as u64);
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if seen.len() > 10_000 {
            seen.retain(|_, at| now.duration_since(*at) < window);
        }
        if seen.get(nonce).is_some_and(|at| now.duration_since(*at) < window) {
            return false;
        }
        seen.insert(nonce.to_string(), now);
        true
    }
}

/// Keys derived from the lattice key.
#[derive(Clone)]
pub struct LatticeKeys {
    discovery: [u8; 32],
    requests: [u8; 32],
}

impl LatticeKeys {
    pub fn new(lattice_key: &[u8; 32]) -> Self {
        LatticeKeys {
            discovery: blake3::derive_key("HexDB 2026 discovery handshake v1", lattice_key),
            requests: blake3::derive_key("HexDB 2026 replication request v1", lattice_key),
        }
    }

    // --- discovery -----------------------------------------------------------

    /// A hello line (without the newline) and the nonce to expect in the reply.
    pub fn hello(&self) -> (String, String) {
        let ts = chrono::Utc::now().timestamp();
        let nonce = new_nonce();
        let tag = mac(&self.discovery, &[b"hello", ts.to_string().as_bytes(), nonce.as_bytes()]);
        (format!("HEXDB_HELLO2 {} {} {}", ts, nonce, tag.to_hex()), nonce)
    }

    /// Check a hello line; returns its nonce if valid, fresh, and unused.
    pub fn check_hello(&self, line: &str, seen: &NonceCache) -> Option<String> {
        let mut parts = line.trim().split(' ');
        if parts.next()? != "HEXDB_HELLO2" {
            return None;
        }
        let ts_text = parts.next()?;
        let nonce = parts.next()?;
        let tag = parse_mac(parts.next()?)?;
        if parts.next().is_some() || nonce.len() != 32 {
            return None;
        }
        let ts: i64 = ts_text.parse().ok()?;
        if !fresh(ts) || mac(&self.discovery, &[b"hello", ts_text.as_bytes(), nonce.as_bytes()]) != tag {
            return None;
        }
        seen.first_use(nonce).then(|| nonce.to_string())
    }

    /// The identity reply line for a hello's nonce.
    pub fn identity_reply(&self, nonce: &str, json: &str) -> String {
        let tag = mac(&self.discovery, &[b"identity", nonce.as_bytes(), json.as_bytes()]);
        format!("HEXDB_IDENTITY {} {}", json, tag.to_hex())
    }

    /// Check an identity reply against our nonce; returns the JSON if genuine.
    pub fn check_identity<'a>(&self, nonce: &str, line: &'a str) -> Option<&'a str> {
        let rest = line.trim_end().strip_prefix("HEXDB_IDENTITY ")?;
        let (json, tag) = rest.rsplit_once(' ')?;
        let tag = parse_mac(tag)?;
        (mac(&self.discovery, &[b"identity", nonce.as_bytes(), json.as_bytes()]) == tag).then_some(json)
    }

    // --- replication requests -------------------------------------------------

    /// The signature header value for a request.
    pub fn sign_request(&self, method: &str, path_and_query: &str, body: &[u8]) -> String {
        let ts = chrono::Utc::now().timestamp();
        let nonce = new_nonce();
        let body_hash = blake3::hash(body);
        let tag = mac(
            &self.requests,
            &[b"request", ts.to_string().as_bytes(), nonce.as_bytes(), method.as_bytes(), path_and_query.as_bytes(), body_hash.as_bytes()],
        );
        format!("{}.{}.{}", ts, nonce, tag.to_hex())
    }

    /// Verify a request signature (fresh, single use, and matching this request).
    pub fn verify_request(&self, header: &str, method: &str, path_and_query: &str, body: &[u8], seen: &NonceCache) -> bool {
        let mut parts = header.trim().split('.');
        let (Some(ts_text), Some(nonce), Some(tag), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return false;
        };
        let (Ok(ts), Some(tag)) = (ts_text.parse::<i64>(), parse_mac(tag)) else { return false };
        if !fresh(ts) || nonce.len() != 32 {
            return false;
        }
        let body_hash = blake3::hash(body);
        let expected = mac(
            &self.requests,
            &[b"request", ts_text.as_bytes(), nonce.as_bytes(), method.as_bytes(), path_and_query.as_bytes(), body_hash.as_bytes()],
        );
        expected == tag && seen.first_use(nonce)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_handshake_is_mutual_and_single_use() {
        let keys = LatticeKeys::new(&[1u8; 32]);
        let stranger = LatticeKeys::new(&[2u8; 32]);
        let seen = NonceCache::default();

        let (hello, nonce) = keys.hello();
        assert!(stranger.check_hello(&hello, &NonceCache::default()).is_none(), "wrong key");
        assert_eq!(keys.check_hello(&hello, &seen).as_deref(), Some(nonce.as_str()));
        assert!(keys.check_hello(&hello, &seen).is_none(), "replayed hello");
        assert!(keys.check_hello("HEXDB_HELLO", &seen).is_none(), "old unauthenticated hello");

        let reply = keys.identity_reply(&nonce, r#"{"id":"a"}"#);
        assert_eq!(keys.check_identity(&nonce, &reply), Some(r#"{"id":"a"}"#));
        assert!(keys.check_identity("another-nonce-0000000000000000000", &reply).is_none(), "bound to the nonce");
        assert!(stranger.check_identity(&nonce, &reply).is_none());
        let forged = reply.replace(r#""a""#, r#""b""#);
        assert!(keys.check_identity(&nonce, &forged).is_none(), "tampered JSON");
    }

    #[test]
    fn request_signatures_bind_the_request() {
        let keys = LatticeKeys::new(&[1u8; 32]);
        let seen = NonceCache::default();
        let sig = keys.sign_request("GET", "/lattice/changes?after=5", b"");
        assert!(!LatticeKeys::new(&[2u8; 32]).verify_request(&sig, "GET", "/lattice/changes?after=5", b"", &NonceCache::default()));
        assert!(!keys.verify_request(&sig, "GET", "/lattice/changes?after=0", b"", &NonceCache::default()), "other path");
        assert!(!keys.verify_request(&sig, "POST", "/lattice/changes?after=5", b"", &NonceCache::default()), "other method");
        assert!(keys.verify_request(&sig, "GET", "/lattice/changes?after=5", b"", &seen));
        assert!(!keys.verify_request(&sig, "GET", "/lattice/changes?after=5", b"", &seen), "replay");

        let body_sig = keys.sign_request("POST", "/lattice/revoke", b"{\"a\":1}");
        assert!(!keys.verify_request(&body_sig, "POST", "/lattice/revoke", b"{\"a\":2}", &NonceCache::default()), "other body");

        let old = format!("{}.{}.{}", chrono::Utc::now().timestamp() - 600, "0".repeat(32), "0".repeat(64));
        assert!(!keys.verify_request(&old, "GET", "/x", b"", &NonceCache::default()));
        assert!(!keys.verify_request("garbage", "GET", "/x", b"", &NonceCache::default()));
    }
}
