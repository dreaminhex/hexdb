//! OAuth access tokens: with `oauth.issuer` and `oauth.audience` set, a JWT
//! from that OpenID Connect provider authenticates as the existing user its
//! claim names; anything else about the token being off refuses it.
//! The provider here is a small local server publishing a P-256 key.

use anyhow::Result;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hexdb_tests::TestServer;
use reqwest::{Method, StatusCode};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const AUDIENCE: &str = "api://hexdb";

struct Provider {
    issuer: String,
    key: EcdsaKeyPair,
    rng: SystemRandom,
    fetches: Arc<AtomicUsize>,
}

impl Provider {
    /// Serve a discovery document and a JWKS with one key ("k1") on a local port.
    fn start() -> Result<Provider> {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let public = key.public_key().as_ref().to_vec();
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let issuer = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        let jwks = json!({ "keys": [{
            "kty": "EC", "crv": "P-256", "kid": "k1", "use": "sig", "alg": "ES256",
            "x": URL_SAFE_NO_PAD.encode(&public[1..33]), "y": URL_SAFE_NO_PAD.encode(&public[33..65]),
        }] });
        let discovery = json!({ "issuer": issuer, "jwks_uri": format!("{}/jwks", issuer) });
        let fetches = Arc::new(AtomicUsize::new(0));
        let counter = fetches.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_err() {
                    continue;
                }
                let body = if line.contains("/jwks") {
                    counter.fetch_add(1, Ordering::SeqCst);
                    jwks.to_string()
                } else {
                    discovery.to_string()
                };
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            }
        });
        Ok(Provider { issuer, key, rng, fetches })
    }

    /// A signed token: these claims over valid defaults (null removes one).
    fn token(&self, overrides: Value) -> String {
        self.token_with(json!({ "alg": "ES256", "kid": "k1", "typ": "JWT" }), overrides)
    }

    fn token_with(&self, header: Value, overrides: Value) -> String {
        let now = chrono::Utc::now().timestamp();
        let mut claims = json!({ "iss": self.issuer, "aud": AUDIENCE, "sub": "user-123", "email": "ada@example.com", "iat": now, "exp": now + 600 });
        for (k, v) in overrides.as_object().unwrap() {
            match v {
                Value::Null => claims.as_object_mut().unwrap().remove(k),
                _ => claims.as_object_mut().unwrap().insert(k.clone(), v.clone()),
            };
        }
        let signing_input = format!("{}.{}", URL_SAFE_NO_PAD.encode(header.to_string()), URL_SAFE_NO_PAD.encode(claims.to_string()));
        let signature = self.key.sign(&self.rng, signing_input.as_bytes()).unwrap();
        format!("{}.{}", signing_input, URL_SAFE_NO_PAD.encode(signature.as_ref()))
    }
}

fn configure(server: &TestServer, changes: Value) -> Result<()> {
    let res = server.request(Method::PUT, "/config", Some(&json!({ "changes": changes })), &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    Ok(())
}

/// The status of reading a document with a token, once the configuration has reached the hex.
fn status_with(server: &TestServer, token: &str, path: &str, expect: StatusCode) -> Result<StatusCode> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = server.request_as(Some(token), Method::GET, path, None, &[])?.status;
        if status == expect || Instant::now() > deadline {
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn provider_tokens_authenticate_as_existing_users() -> Result<()> {
    let server = TestServer::start()?;
    let provider = Provider::start()?;
    let id = server.insert("notes", &json!({ "title": "hello" }))?;
    let doc = format!("/notes/{}", id);
    server.user_with_roles("ada", json!([{ "name": "reader", "tessellations": ["notes"] }]))?;

    // Off until configured: a JWT is just an unknown credential.
    let token = provider.token(json!({}));
    assert_eq!(server.request_as(Some(&token), Method::GET, &doc, None, &[])?.status, StatusCode::UNAUTHORIZED);

    configure(&server, json!({ "oauth.issuer": provider.issuer, "oauth.audience": AUDIENCE }))?;
    assert_eq!(status_with(&server, &token, &doc, StatusCode::OK)?, StatusCode::OK);
    let me = server.request_as(Some(&token), Method::GET, "/auth/me", None, &[])?;
    assert_eq!(me.body["login"], "ada", "{}", me.body);
    assert_eq!(me.body["credential"]["kind"], "oauth", "{}", me.body);
    // The user's roles apply: a reader can't write.
    let write = server.request_as(Some(&token), Method::POST, "/notes", Some(&json!({ "title": "no" })), &[])?;
    assert_eq!(write.status, StatusCode::FORBIDDEN, "{}", write.body);
    // The email claim matches ignoring case; keys are fetched once and reused.
    let upper = provider.token(json!({ "email": "ADA@Example.com" }));
    assert_eq!(server.request_as(Some(&upper), Method::GET, &doc, None, &[])?.status, StatusCode::OK);
    assert_eq!(provider.fetches.load(Ordering::SeqCst), 1);

    // Refused: another audience or issuer, expired, not yet valid, no or unknown identity,
    // a forged signature, and unsigned or shared-secret algorithms.
    let forged = {
        let mut parts: Vec<String> = provider.token(json!({})).split('.').map(String::from).collect();
        parts[1] = URL_SAFE_NO_PAD.encode(json!({ "iss": provider.issuer, "aud": AUDIENCE, "email": "admin@example.com", "exp": chrono::Utc::now().timestamp() + 600 }).to_string());
        parts.join(".")
    };
    let now = chrono::Utc::now().timestamp();
    for (why, bad) in [
        ("audience", provider.token(json!({ "aud": "api://other" }))),
        ("issuer", provider.token(json!({ "iss": "https://evil.example.com" }))),
        ("expired", provider.token(json!({ "exp": now - 120 }))),
        ("no exp", provider.token(json!({ "exp": null }))),
        ("nbf", provider.token(json!({ "nbf": now + 600 }))),
        ("no email", provider.token(json!({ "email": null }))),
        ("unknown user", provider.token(json!({ "email": "eve@example.com" }))),
        ("forged", forged),
        ("alg none", provider.token_with(json!({ "alg": "none", "kid": "k1" }), json!({}))),
        ("HS256", provider.token_with(json!({ "alg": "HS256", "kid": "k1" }), json!({}))),
    ] {
        let res = server.request_as(Some(&bad), Method::GET, &doc, None, &[])?;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{}: {}", why, res.body);
    }

    // Machine clients: the sub claim names a user created for the client.
    configure(&server, json!({ "oauth.user_claim": "sub" }))?;
    server.user_with_roles("billing-agent", json!([{ "name": "reader", "tessellations": ["notes"] }]))?;
    let agent = provider.token(json!({ "sub": "billing-agent", "email": null }));
    assert_eq!(status_with(&server, &agent, &doc, StatusCode::OK)?, StatusCode::OK);
    let me = server.request_as(Some(&agent), Method::GET, "/auth/me", None, &[])?;
    assert_eq!(me.body["login"], "billing-agent", "{}", me.body);

    // Discovery: the protected resource metadata names the provider, a 401 points to it,
    // and the A2A agent card offers the provider's tokens beside API keys.
    let metadata = server.request_as(None, Method::GET, "/.well-known/oauth-protected-resource", None, &[])?;
    assert_eq!(metadata.status, StatusCode::OK, "{}", metadata.body);
    assert_eq!(metadata.body["authorization_servers"], json!([provider.issuer]));
    let anonymous = server.request_as(None, Method::GET, "/notes", None, &[])?;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    let challenge = anonymous.headers["www-authenticate"].to_str()?.to_string();
    assert!(challenge.contains("resource_metadata=") && challenge.contains("/.well-known/oauth-protected-resource"), "{}", challenge);
    configure(&server, json!({ "ai.chat.enabled": true, "a2a.enabled": true }))?;
    let card = server.request_as(None, Method::GET, "/.well-known/agent-card.json", None, &[])?;
    assert_eq!(card.status, StatusCode::OK, "{}", card.body);
    assert_eq!(card.body["securitySchemes"]["oidc"]["openIdConnectSecurityScheme"]["openIdConnectUrl"], format!("{}/.well-known/openid-configuration", provider.issuer));
    assert_eq!(card.body["securityRequirements"].as_array().map(Vec::len), Some(2), "either scheme: {}", card.body);

    // Turned off again: tokens stop working, and discovery says nothing.
    configure(&server, json!({ "oauth.issuer": "" }))?;
    assert_eq!(status_with(&server, &agent, &doc, StatusCode::UNAUTHORIZED)?, StatusCode::UNAUTHORIZED);
    assert_eq!(server.request_as(None, Method::GET, "/.well-known/oauth-protected-resource", None, &[])?.status, StatusCode::NOT_FOUND);
    Ok(())
}
