// HexDB Core OAuth: access tokens from an OpenID Connect provider
//
// With `oauth.issuer` and `oauth.audience` set in the lattice configuration,
// a bearer token that is a JWT is checked against that provider (Microsoft
// Entra ID, Auth0, Okta, Keycloak, Google, ...):
//
// - the signature, with the provider's keys: its discovery document
//   (`<issuer>/.well-known/openid-configuration`) names its JWKS, fetched and
//   cached for an hour, and again (at most once a minute) for a key ID it
//   doesn't have. RS256/384/512, PS256/384/512, ES256/384 and EdDSA; never
//   `none` or a shared-secret HMAC.
// - `iss` is the provider's issuer, `aud` names `oauth.audience`, and the
//   token is within `nbf`..`exp` (a minute's leeway for clocks).
//
// The request then runs as the existing HexDB user whose login, or else
// email address, equals the token's `oauth.user_claim` (default `email`),
// ignoring case. Unknown identities are refused: HexDB never creates users
// from tokens, so administrators decide who gets in and with which roles.
// HexDB is not an authorization server; it only accepts the provider's tokens.

use crate::auth::{Credential, Principal};
use crate::users;
use crate::HexDBEngine;
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ring::signature::{self, UnparsedPublicKey};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Longest token accepted, in bytes.
const MAX_TOKEN_BYTES: usize = 16 * 1024;
/// How long fetched keys are used before they're fetched again.
const KEYS_TTL: Duration = Duration::from_secs(3600);
/// Least time between fetches for a key ID the cached keys don't have.
const REFETCH_GAP: Duration = Duration::from_secs(60);
/// Clock leeway for `exp` and `nbf`, in seconds.
const LEEWAY: i64 = 60;
/// Verified tokens remembered (so the user isn't looked up by claim per request).
const VERIFIED_CAPACITY: usize = 4096;

/// The provider settings, from the lattice configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub issuer: String,
    pub audience: String,
    pub user_claim: String,
}

impl Settings {
    /// The configured provider, or None while `oauth.issuer` or `oauth.audience` is empty.
    pub fn from_engine(engine: &HexDBEngine) -> Option<Settings> {
        let text = |key: &str| engine.config_value(key).as_str().map(str::trim).unwrap_or_default().to_string();
        let (issuer, audience, user_claim) = (text("oauth.issuer"), text("oauth.audience"), text("oauth.user_claim"));
        if issuer.is_empty() || audience.is_empty() {
            return None;
        }
        Some(Settings { issuer, audience, user_claim: if user_claim.is_empty() { "email".into() } else { user_claim } })
    }

    /// The provider's discovery document.
    pub fn discovery_url(&self) -> String {
        format!("{}/.well-known/openid-configuration", self.issuer.trim_end_matches('/'))
    }
}

/// True for a string shaped like a JWT (three base64url parts, a JSON header).
pub fn looks_like_jwt(token: &str) -> bool {
    token.starts_with("eyJ") && token.split('.').count() == 3
}

/// One verification key from the provider's JWKS.
#[derive(Debug, Clone)]
enum Key {
    Rsa { n: Vec<u8>, e: Vec<u8> },
    Ec { curve: &'static str, point: Vec<u8> },
    Ed25519(Vec<u8>),
}

#[derive(Debug, Clone)]
struct Jwk {
    kid: Option<String>,
    alg: Option<String>,
    key: Key,
}

struct Keys {
    /// The settings these keys were fetched for.
    settings: Settings,
    /// The issuer the discovery document names (tokens' `iss` must equal it).
    issuer: String,
    keys: Vec<Jwk>,
    fetched: Instant,
}

/// Provider keys and verified tokens, shared by every request on this hex.
#[derive(Default)]
pub struct OAuth {
    keys: tokio::sync::RwLock<Option<Keys>>,
    /// Serializes fetches, and spaces out fetches for unknown key IDs.
    fetching: tokio::sync::Mutex<Option<Instant>>,
    /// Token hash -> (user ID, expiry in epoch seconds), for the settings in `keys`.
    verified: Mutex<HashMap<[u8; 32], (String, i64)>>,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

impl OAuth {
    /// The principal for a provider's access token, or None if it isn't valid
    /// for this lattice (the reason is logged, not returned to the caller).
    pub async fn authenticate(&self, engine: &HexDBEngine, token: &str) -> Result<Option<Principal>> {
        let Some(settings) = Settings::from_engine(engine) else { return Ok(None) };
        if token.len() > MAX_TOKEN_BYTES {
            return Ok(None);
        }
        let hash = *blake3::hash(token.as_bytes()).as_bytes();
        let now = chrono::Utc::now().timestamp();
        let cached = self.verified.lock().unwrap().get(&hash).cloned();
        let (user_id, subject) = match cached {
            Some((user_id, exp)) if exp + LEEWAY > now && self.current_for(&settings).await => (user_id, None),
            _ => match self.verify(engine, &settings, token, now).await {
                Ok(found) => found,
                Err(e) => {
                    info!("🔑 OAuth token refused: {:#}", e);
                    return Ok(None);
                }
            },
        };
        let Some((id, user)) = users::find_by_id(engine, &user_id).await? else { return Ok(None) };
        if user.is_locked {
            return Ok(None);
        }
        let definitions = users::role_definitions(engine).await?;
        let subject = subject.unwrap_or_else(|| user.login.clone());
        Ok(Some(
            Principal::new(
                id.to_string(),
                user.login,
                user.email_address,
                user.roles,
                Credential::OAuth { issuer: settings.issuer.clone(), subject },
                &definitions,
            )
            .with_attributes(user.attributes),
        ))
    }

    /// Whether the cached keys (and so the remembered tokens) are for these settings.
    async fn current_for(&self, settings: &Settings) -> bool {
        self.keys.read().await.as_ref().is_some_and(|k| k.settings == *settings)
    }

    /// Check a token and find its user: (user ID, the token's subject).
    async fn verify(&self, engine: &HexDBEngine, settings: &Settings, token: &str, now: i64) -> Result<(String, Option<String>)> {
        let mut parts = token.split('.');
        let (Some(head), Some(body), Some(sig)) = (parts.next(), parts.next(), parts.next()) else { bail!("not a JWT") };
        let header: Header = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(head).context("header isn't base64url")?).context("unreadable header")?;
        let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).context("claims aren't base64url")?).context("unreadable claims")?;
        let signature = URL_SAFE_NO_PAD.decode(sig).context("signature isn't base64url")?;
        let message = &token.as_bytes()[..head.len() + 1 + body.len()];

        let issuer = self.check_signature(settings, &header, message, &signature).await?;
        if claims["iss"].as_str() != Some(issuer.as_str()) {
            bail!("iss is {}, not {}", claims["iss"], issuer);
        }
        let audience_ok = match &claims["aud"] {
            Value::String(aud) => *aud == settings.audience,
            Value::Array(list) => list.iter().any(|a| a.as_str() == Some(settings.audience.as_str())),
            _ => false,
        };
        if !audience_ok {
            bail!("aud {} doesn't name {}", claims["aud"], settings.audience);
        }
        let exp = claims["exp"].as_i64().ok_or_else(|| anyhow!("no exp"))?;
        if exp + LEEWAY <= now {
            bail!("expired");
        }
        if claims["nbf"].as_i64().is_some_and(|nbf| nbf - LEEWAY > now) {
            bail!("not valid yet (nbf)");
        }

        let identity = claims[settings.user_claim.as_str()].as_str().filter(|v| !v.is_empty());
        let identity = identity.ok_or_else(|| anyhow!("no {} claim", settings.user_claim))?;
        let user_id = find_user(engine, identity).await?.ok_or_else(|| anyhow!("no HexDB user has the login or email address {}", identity))?;
        let mut verified = self.verified.lock().unwrap();
        if verified.len() >= VERIFIED_CAPACITY {
            verified.retain(|_, (_, e)| *e + LEEWAY > now);
            if verified.len() >= VERIFIED_CAPACITY {
                verified.clear();
            }
        }
        verified.insert(*blake3::hash(token.as_bytes()).as_bytes(), (user_id.clone(), exp));
        Ok((user_id, claims["sub"].as_str().map(String::from)))
    }

    /// Verify the signature with the provider's keys; returns the provider's issuer.
    async fn check_signature(&self, settings: &Settings, header: &Header, message: &[u8], signature: &[u8]) -> Result<String> {
        let fresh = self.keys.read().await.as_ref().is_some_and(|k| k.settings == *settings && k.fetched.elapsed() < KEYS_TTL);
        if !fresh {
            if let Err(e) = self.fetch_keys(settings).await {
                // Keys fetched earlier keep working while the provider can't be reached.
                if !self.current_for(settings).await {
                    return Err(e);
                }
                warn!("🔑 OAuth: couldn't refresh the provider's keys; using the ones fetched earlier: {:#}", e);
            }
        }
        if let Some(result) = self.try_keys(settings, header, message, signature).await {
            return result;
        }
        // A key ID the cached keys don't have: the provider may have rotated its keys.
        self.fetch_keys(settings).await?;
        match self.try_keys(settings, header, message, signature).await {
            Some(result) => result,
            None => bail!("no provider key {} for {}", header.kid.as_deref().unwrap_or("(no kid)"), header.alg),
        }
    }

    /// Check the signature with the cached keys that fit the header (None: no key fits).
    async fn try_keys(&self, settings: &Settings, header: &Header, message: &[u8], signature: &[u8]) -> Option<Result<String>> {
        let keys = self.keys.read().await;
        let keys = keys.as_ref().filter(|k| k.settings == *settings)?;
        let candidates: Vec<&Jwk> = keys
            .keys
            .iter()
            .filter(|k| header.kid.is_none() || k.kid == header.kid)
            .filter(|k| k.alg.as_deref().is_none_or(|a| a == header.alg))
            .collect();
        if candidates.is_empty() {
            return None;
        }
        Some(match candidates.iter().any(|k| verify_with(&k.key, &header.alg, message, signature)) {
            true => Ok(keys.issuer.clone()),
            false => Err(anyhow!("bad signature ({})", header.alg)),
        })
    }

    /// Fetch the provider's discovery document and keys (unless another
    /// request just did, or a fetch for these settings was under a minute ago).
    async fn fetch_keys(&self, settings: &Settings) -> Result<()> {
        let mut last = self.fetching.lock().await;
        let current = self.current_for(settings).await;
        if current && last.is_some_and(|t| t.elapsed() < REFETCH_GAP) {
            return Ok(());
        }
        *last = Some(Instant::now());
        let client = crate::crypt::http_client().timeout(Duration::from_secs(10)).build()?;
        let discovery: Value = fetch_json(&client, &settings.discovery_url()).await?;
        let issuer = discovery["issuer"].as_str().ok_or_else(|| anyhow!("the discovery document has no issuer"))?.to_string();
        if issuer.trim_end_matches('/') != settings.issuer.trim_end_matches('/') {
            bail!("the discovery document's issuer is {}, not oauth.issuer ({})", issuer, settings.issuer);
        }
        let jwks_uri = discovery["jwks_uri"].as_str().ok_or_else(|| anyhow!("the discovery document has no jwks_uri"))?;
        let jwks: Value = fetch_json(&client, jwks_uri).await?;
        let keys: Vec<Jwk> = jwks["keys"].as_array().map(|list| list.iter().filter_map(parse_jwk).collect()).unwrap_or_default();
        if keys.is_empty() {
            warn!("🔑 OAuth: {} has no usable signing keys.", jwks_uri);
        }
        let changed = !current;
        *self.keys.write().await = Some(Keys { settings: settings.clone(), issuer, keys, fetched: Instant::now() });
        if changed {
            self.verified.lock().unwrap().clear();
            info!("🔑 OAuth: using {} for access tokens.", settings.issuer);
        }
        Ok(())
    }
}

/// GET a JSON document over HTTPS (plain HTTP only on this machine, for testing).
async fn fetch_json(client: &reqwest::Client, url: &str) -> Result<Value> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("bad URL {}", url))?;
    let local = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && local) {
        bail!("{} isn't https", url);
    }
    let response = client.get(parsed).send().await.with_context(|| format!("couldn't reach {}", url))?;
    if !response.status().is_success() {
        bail!("{} answered {}", url, response.status());
    }
    response.json().await.with_context(|| format!("{} isn't JSON", url))
}

/// A user by login, or else by email address, ignoring case.
async fn find_user(engine: &HexDBEngine, identity: &str) -> Result<Option<String>> {
    if let Some((id, _)) = users::find_for_login(engine, identity).await? {
        return Ok(Some(id.to_string()));
    }
    let wanted = identity.to_lowercase();
    let mut matches = users::list_users(engine).await?.into_iter().filter(|u| u.email_address.to_lowercase() == wanted);
    match (matches.next(), matches.next()) {
        (Some(user), None) => Ok(Some(user.id)),
        // Two users with one address: which one is meant is unclear, so neither.
        _ => Ok(None),
    }
}

fn b64(value: &Value) -> Option<Vec<u8>> {
    value.as_str().and_then(|s| URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok())
}

/// A signing key from a JWKS entry (encryption keys and unknown types are skipped).
fn parse_jwk(jwk: &Value) -> Option<Jwk> {
    if jwk["use"].as_str().is_some_and(|u| u != "sig") {
        return None;
    }
    let key = match (jwk["kty"].as_str()?, jwk["crv"].as_str()) {
        ("RSA", _) => Key::Rsa { n: b64(&jwk["n"])?, e: b64(&jwk["e"])? },
        ("EC", Some(curve @ ("P-256" | "P-384"))) => {
            let (x, y) = (b64(&jwk["x"])?, b64(&jwk["y"])?);
            let mut point = vec![0x04];
            point.extend(x);
            point.extend(y);
            Key::Ec { curve: if curve == "P-256" { "P-256" } else { "P-384" }, point }
        }
        ("OKP", Some("Ed25519")) => Key::Ed25519(b64(&jwk["x"])?),
        _ => return None,
    };
    Some(Jwk { kid: jwk["kid"].as_str().map(String::from), alg: jwk["alg"].as_str().map(String::from), key })
}

/// Check a signature with one key, for the header's algorithm.
fn verify_with(key: &Key, alg: &str, message: &[u8], sig: &[u8]) -> bool {
    match key {
        Key::Rsa { n, e } => {
            let params: &signature::RsaParameters = match alg {
                "RS256" => &signature::RSA_PKCS1_2048_8192_SHA256,
                "RS384" => &signature::RSA_PKCS1_2048_8192_SHA384,
                "RS512" => &signature::RSA_PKCS1_2048_8192_SHA512,
                "PS256" => &signature::RSA_PSS_2048_8192_SHA256,
                "PS384" => &signature::RSA_PSS_2048_8192_SHA384,
                "PS512" => &signature::RSA_PSS_2048_8192_SHA512,
                _ => return false,
            };
            signature::RsaPublicKeyComponents { n, e }.verify(params, message, sig).is_ok()
        }
        Key::Ec { curve, point } => {
            let algorithm: &'static dyn signature::VerificationAlgorithm = match (alg, *curve) {
                ("ES256", "P-256") => &signature::ECDSA_P256_SHA256_FIXED,
                ("ES384", "P-384") => &signature::ECDSA_P384_SHA384_FIXED,
                _ => return false,
            };
            UnparsedPublicKey::new(algorithm, point).verify(message, sig).is_ok()
        }
        Key::Ed25519(x) => alg == "EdDSA" && UnparsedPublicKey::new(&signature::ED25519, x).verify(message, sig).is_ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};

    #[test]
    fn es256_signatures_verify_and_tampering_fails() {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let public = pair.public_key().as_ref();
        let jwk = serde_json::json!({
            "kty": "EC", "crv": "P-256", "kid": "k1",
            "x": URL_SAFE_NO_PAD.encode(&public[1..33]), "y": URL_SAFE_NO_PAD.encode(&public[33..65]),
        });
        let key = parse_jwk(&jwk).unwrap().key;
        let sig = pair.sign(&rng, b"header.claims").unwrap();
        assert!(verify_with(&key, "ES256", b"header.claims", sig.as_ref()));
        assert!(!verify_with(&key, "ES256", b"header.claimz", sig.as_ref()));
        assert!(!verify_with(&key, "ES384", b"header.claims", sig.as_ref()), "the algorithm must fit the key");
        assert!(!verify_with(&key, "none", b"header.claims", b""));
        assert!(!verify_with(&key, "HS256", b"header.claims", sig.as_ref()));
        assert!(parse_jwk(&serde_json::json!({ "kty": "RSA", "use": "enc", "n": "AQAB", "e": "AQAB" })).is_none(), "encryption keys aren't used");
    }

    #[test]
    fn jwts_are_recognized() {
        assert!(looks_like_jwt("eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ4In0.c2ln"));
        assert!(!looks_like_jwt("hxk_abc.def"));
        assert!(!looks_like_jwt("eyJ.only-two"));
    }
}
