//! Security: authentication, authorization, sessions, API keys, CSRF and
//! header protections, lattice authentication, and encryption at rest.

use anyhow::Result;
use hexdb_tests::{TestOptions, TestServer, TEST_ADMIN_LOGIN, TEST_ADMIN_PASSWORD};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};

fn login(server: &TestServer, login: &str, password: &str) -> Result<hexdb_tests::ApiResponse> {
    server.request_as(None, Method::POST, "/auth/login", Some(&json!({ "login": login, "password": password, "return_token": true })), &[])
}

#[test]
fn every_private_route_requires_credentials() -> Result<()> {
    let server = TestServer::start()?;
    let id = server.insert("notes", &json!({ "a": 1 }))?;
    let routes: Vec<(Method, String)> = vec![
        (Method::GET, "/status".into()),
        (Method::GET, "/status/history".into()),
        (Method::GET, "/logs".into()),
        (Method::GET, "/plugins".into()),
        (Method::GET, "/changes".into()),
        (Method::GET, "/changes/stream".into()),
        (Method::POST, "/flush".into()),
        (Method::POST, "/transactions".into()),
        (Method::POST, "/graphql".into()),
        (Method::GET, "/tessellations".into()),
        (Method::POST, "/tessellations".into()),
        (Method::GET, "/tessellations/notes".into()),
        (Method::DELETE, "/tessellations/notes".into()),
        (Method::GET, "/tessellations/notes/indexes".into()),
        (Method::POST, "/tessellations/notes/indexes".into()),
        (Method::DELETE, "/tessellations/notes/indexes/x".into()),
        (Method::GET, "/users".into()),
        (Method::POST, "/users".into()),
        (Method::GET, "/users/admin".into()),
        (Method::PATCH, "/users/admin".into()),
        (Method::DELETE, "/users/admin".into()),
        (Method::GET, "/roles".into()),
        (Method::GET, "/roles/admin".into()),
        (Method::GET, "/auth/me".into()),
        (Method::POST, "/auth/logout".into()),
        (Method::POST, "/auth/password".into()),
        (Method::GET, "/auth/keys".into()),
        (Method::POST, "/auth/keys".into()),
        (Method::DELETE, format!("/auth/keys/{}", id)),
        (Method::GET, "/notes".into()),
        (Method::POST, "/notes".into()),
        (Method::GET, "/notes/count".into()),
        (Method::POST, "/notes/_query".into()),
        (Method::POST, "/notes/_aggregate".into()),
        (Method::POST, "/notes/_bulk".into()),
        (Method::PUT, "/notes/_bulk".into()),
        (Method::PATCH, "/notes/_bulk".into()),
        (Method::POST, "/notes/_update".into()),
        (Method::GET, format!("/notes/{}", id)),
        (Method::PUT, format!("/notes/{}", id)),
        (Method::PATCH, format!("/notes/{}", id)),
        (Method::DELETE, format!("/notes/{}", id)),
        (Method::GET, "/no-such-route/at/all".into()),
    ];
    let forged = {
        // A token signed with the wrong key.
        let real = server.token();
        let (payload, _) = real.rsplit_once('.').unwrap();
        format!("{}.{}", payload, "A".repeat(43))
    };
    for (method, path) in &routes {
        for token in [None, Some("hxs.garbage.garbage"), Some(forged.as_str()), Some("hxk_01ARZ3NDEKTSV4RRFFQ69G5FAV_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")] {
            let res = server.request_as(token, method.clone(), path, Some(&json!({})), &[])?;
            assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{} {} with {:?} -> {}", method, path, token.map(|t| &t[..8.min(t.len())]), res.body);
        }
    }
    // Garbage cookies are refused too, and cleared.
    let res = server.request_as(None, Method::GET, "/tessellations", None, &[("Cookie", "hexdb_session=nope")])?;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    assert!(res.headers["set-cookie"].to_str()?.contains("Max-Age=0"));

    // Health is public but says nothing about the hex without credentials.
    let anon = server.request_as(None, Method::GET, "/health", None, &[])?;
    assert_eq!(anon.body, json!({ "status": "ok" }));
    let authed = server.request(Method::GET, "/health", None, &[])?;
    assert!(authed.body["version"].is_string());

    // The documents themselves were never touched.
    assert_eq!(server.count("notes")?, 1);
    Ok(())
}

#[test]
fn sign_in_is_generic_throttled_and_never_leaks_tokens_to_scripts() -> Result<()> {
    let server = TestServer::start()?;
    server.insert("notes", &json!({}))?;

    let wrong = login(&server, TEST_ADMIN_LOGIN, "not the password at all")?;
    let unknown = login(&server, "nobody-here", "not the password at all")?;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.body, unknown.body, "unknown logins look like wrong passwords");

    // Without return_token, the token is only in an HttpOnly, SameSite=Strict cookie.
    let res = server.request_as(None, Method::POST, "/auth/login", Some(&json!({ "login": TEST_ADMIN_LOGIN, "password": TEST_ADMIN_PASSWORD })), &[])?;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.get("token").is_none(), "{}", res.body);
    let cookie = res.headers["set-cookie"].to_str()?.to_string();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict") && cookie.contains("Path=/"), "{}", cookie);
    let session = cookie.split(';').next().unwrap().to_string();

    // Cookie sessions read freely, but changes must come from the same origin (CSRF).
    let headers = |extra: &[(&'static str, &'static str)]| -> Vec<(&'static str, String)> {
        let mut h: Vec<(&'static str, String)> = vec![("Cookie", session.clone())];
        h.extend(extra.iter().map(|(k, v)| (*k, v.to_string())));
        h
    };
    let call = |method: Method, path: &str, extra: &[(&'static str, &'static str)]| -> Result<StatusCode> {
        let h = headers(extra);
        let refs: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
        Ok(server.request_as(None, method, path, Some(&json!({ "x": 1 })), &refs)?.status)
    };
    assert_eq!(call(Method::GET, "/notes", &[])?, StatusCode::OK);
    assert_eq!(call(Method::POST, "/notes", &[("Origin", "https://evil.example")])?, StatusCode::FORBIDDEN);
    assert_eq!(call(Method::POST, "/notes", &[])?, StatusCode::FORBIDDEN, "no Origin and no Sec-Fetch-Site");
    assert_eq!(call(Method::POST, "/notes", &[("Sec-Fetch-Site", "cross-site")])?, StatusCode::FORBIDDEN);
    let origin = server.url("").trim_end_matches('/').to_string();
    let h = headers(&[]);
    let mut refs: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
    refs.push(("Origin", origin.as_str()));
    assert_eq!(server.request_as(None, Method::POST, "/notes", Some(&json!({ "x": 1 })), &refs)?.status, StatusCode::CREATED);
    // Cross-site sign-in is refused (login CSRF).
    let res = server.request_as(None, Method::POST, "/auth/login", Some(&json!({ "login": TEST_ADMIN_LOGIN, "password": TEST_ADMIN_PASSWORD })), &[("Origin", "https://evil.example")])?;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Repeated failures lock sign-in for that login and address, even with the right password.
    for _ in 0..5 {
        login(&server, TEST_ADMIN_LOGIN, "still not the password")?;
    }
    let blocked = login(&server, TEST_ADMIN_LOGIN, TEST_ADMIN_PASSWORD)?;
    assert_eq!(blocked.status, StatusCode::TOO_MANY_REQUESTS, "{}", blocked.body);
    assert!(blocked.headers.contains_key("retry-after"));
    // Existing sessions keep working.
    assert_eq!(server.request(Method::GET, "/notes", None, &[])?.status, StatusCode::OK);
    Ok(())
}

#[test]
fn sessions_end_on_sign_out_password_change_lock_and_role_loss() -> Result<()> {
    let server = TestServer::start()?;
    server.insert("notes", &json!({}))?;
    server.request(
        Method::POST,
        "/users",
        Some(&json!({ "login": "ada", "password": "Sturdy test passphrase 2026", "email_address": "ada@example.com", "roles": [{ "name": "reader", "permissions": ["notes"] }] })),
        &[],
    )?;
    let token = |s: &hexdb_tests::ApiResponse| s.body["token"].as_str().unwrap().to_string();
    let get = |t: &str| server.request_as(Some(t), Method::GET, "/notes", None, &[]).map(|r| r.status);

    // Sign-out revokes the session.
    let a = token(&login(&server, "ada", "Sturdy test passphrase 2026")?);
    assert_eq!(get(&a)?, StatusCode::OK);
    assert_eq!(server.request_as(Some(&a), Method::POST, "/auth/logout", None, &[])?.status, StatusCode::NO_CONTENT);
    assert_eq!(get(&a)?, StatusCode::UNAUTHORIZED);

    // A password change signs out every other session; the new password works.
    let (b, c) = (token(&login(&server, "ada", "Sturdy test passphrase 2026")?), token(&login(&server, "ada", "Sturdy test passphrase 2026")?));
    let wrong = server.request_as(Some(&b), Method::POST, "/auth/password", Some(&json!({ "current_password": "wrong", "new_password": "Brand new passphrase 2026" })), &[])?;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);
    let weak = server.request_as(Some(&b), Method::POST, "/auth/password", Some(&json!({ "current_password": "Sturdy test passphrase 2026", "new_password": "short" })), &[])?;
    assert_eq!(weak.status, StatusCode::BAD_REQUEST);
    let changed = server.request_as(Some(&b), Method::POST, "/auth/password", Some(&json!({ "current_password": "Sturdy test passphrase 2026", "new_password": "Brand new passphrase 2026" })), &[])?;
    assert_eq!(changed.status, StatusCode::NO_CONTENT, "{}", changed.body);
    assert_eq!(get(&b)?, StatusCode::UNAUTHORIZED);
    assert_eq!(get(&c)?, StatusCode::UNAUTHORIZED);
    let fresh = changed.headers["set-cookie"].to_str()?.split(';').next().unwrap().to_string();
    assert_eq!(server.request_as(None, Method::GET, "/notes", None, &[("Cookie", &fresh)])?.status, StatusCode::OK, "the changing session gets a new cookie");
    assert_eq!(login(&server, "ada", "Sturdy test passphrase 2026")?.status, StatusCode::UNAUTHORIZED);
    let d = token(&login(&server, "ada", "Brand new passphrase 2026")?);

    // Role changes apply to existing sessions at once.
    server.request(Method::PATCH, "/users/ada", Some(&json!({ "roles": [] })), &[])?;
    assert_eq!(get(&d)?, StatusCode::FORBIDDEN);
    server.request(Method::PATCH, "/users/ada", Some(&json!({ "roles": [{ "name": "reader", "permissions": ["notes"] }] })), &[])?;
    assert_eq!(get(&d)?, StatusCode::OK);

    // Locking signs the user out, and sign-in is refused.
    server.request(Method::PATCH, "/users/ada", Some(&json!({ "is_locked": true })), &[])?;
    assert_eq!(get(&d)?, StatusCode::UNAUTHORIZED);
    assert_eq!(login(&server, "ada", "Brand new passphrase 2026")?.status, StatusCode::UNAUTHORIZED);
    Ok(())
}

#[test]
fn roles_grant_exactly_their_permissions() -> Result<()> {
    let server = TestServer::start()?;
    let note = server.insert("notes", &json!({ "text": "hello" }))?;
    let secret = server.insert("secret", &json!({ "pin": 1234 }))?;
    let reader = server.user_with_roles("rita", json!([{ "name": "reader", "permissions": ["notes"] }]))?;
    let writer = server.user_with_roles("will", json!([{ "name": "writer", "permissions": ["notes"] }]))?;
    let owner = server.user_with_roles("olga", json!([{ "name": "owner", "permissions": ["notes"] }]))?;

    let status = |key: &str, method: Method, path: &str, body: Value| -> Result<StatusCode> {
        Ok(server.request_as(Some(key), method, path, Some(&body), &[])?.status)
    };
    use Method as M;
    let note_path = format!("/notes/{}", note);
    let secret_path = format!("/secret/{}", secret);
    let tx_write = json!({ "operations": [{ "op": "insert", "tessellation": "notes", "data": {} }] });
    let tx_secret = json!({ "operations": [{ "op": "get", "tessellation": "secret", "id": secret }] });
    let cases: Vec<(&str, M, &str, Value, [u16; 3])> = vec![
        // (description, method, path, body, [reader, writer, owner])
        ("read notes", M::GET, &note_path, json!(null), [200, 200, 200]),
        ("query notes", M::POST, "/notes/_query", json!({}), [200, 200, 200]),
        ("aggregate notes", M::POST, "/notes/_aggregate", json!({}), [200, 200, 200]),
        ("read secret", M::GET, &secret_path, json!(null), [403, 403, 403]),
        ("count secret", M::GET, "/secret/count", json!(null), [403, 403, 403]),
        ("insert notes", M::POST, "/notes", json!({ "x": 1 }), [403, 201, 201]),
        ("patch notes", M::PATCH, &note_path, json!({ "x": 2 }), [403, 200, 200]),
        ("transaction write", M::POST, "/transactions", tx_write, [403, 200, 200]),
        ("transaction secret", M::POST, "/transactions", tx_secret, [403, 403, 403]),
        ("insert secret", M::POST, "/secret", json!({}), [403, 403, 403]),
        ("create tessellation", M::POST, "/tessellations", json!({ "name": "newone" }), [403, 403, 403]),
        ("create index", M::POST, "/tessellations/notes/indexes", json!({ "fields": ["x"] }), [403, 403, 201]),
        ("list users", M::GET, "/users", json!(null), [403, 403, 403]),
        ("create admin", M::POST, "/users", json!({ "login": "evil", "password": "Sturdy test passphrase 2026", "email_address": "e@x.io", "roles": [{ "name": "admin", "permissions": ["*"] }] }), [403, 403, 403]),
        ("status", M::GET, "/status", json!(null), [403, 403, 403]),
        ("logs", M::GET, "/logs", json!(null), [403, 403, 403]),
        ("plugins", M::GET, "/plugins", json!(null), [403, 403, 403]),
        ("flush", M::POST, "/flush", json!(null), [403, 403, 403]),
        ("all keys", M::GET, "/auth/keys?all=true", json!(null), [403, 403, 403]),
        ("shutdown", M::POST, "/shutdown", json!(null), [403, 403, 403]),
    ];
    for (what, method, path, body, expected) in cases {
        for (key, want, who) in [(&reader, expected[0], "reader"), (&writer, expected[1], "writer"), (&owner, expected[2], "owner")] {
            let got = status(key, method.clone(), path, body.clone())?;
            assert_eq!(got.as_u16(), want, "{} as {}", what, who);
        }
    }
    // Deleting the tessellation needs owner (checked last: it removes the data).
    assert_eq!(status(&writer, M::DELETE, "/tessellations/notes", json!(null))?.as_u16(), 403);

    // Lists only show what the caller can read.
    let list = server.request_as(Some(&reader), M::GET, "/tessellations", None, &[])?;
    let names: Vec<&str> = list.body["tessellations"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["notes"]);
    let changes = server.request_as(Some(&reader), M::GET, "/changes?after=0", None, &[]);
    let _ = changes; // history may predate startup; covered below with a fresh cursor
    let cursor = server.request_as(Some(&reader), M::GET, "/changes", None, &[])?.body["last_seq"].as_u64().unwrap();
    server.insert("secret", &json!({ "pin": 9 }))?;
    server.insert("notes", &json!({ "visible": true }))?;
    let feed = server.request_as(Some(&reader), M::GET, &format!("/changes?after={}", cursor), None, &[])?;
    let tess: Vec<&str> = feed.body["changes"].as_array().unwrap().iter().map(|c| c["tessellation"].as_str().unwrap()).collect();
    assert_eq!(tess, ["notes"], "the feed hides tessellations the reader can't read");
    assert_eq!(status(&reader, M::GET, "/changes?tessellation=secret", json!(null))?.as_u16(), 403);

    // GraphQL enforces the same rules.
    let gql = server.request_as(Some(&reader), M::POST, "/graphql", Some(&json!({ "query": "{ documents(tessellation: \"secret\") { total } }" })), &[])?;
    assert_eq!(gql.body["errors"][0]["extensions"]["code"], "FORBIDDEN");
    assert_eq!(status(&owner, M::DELETE, "/tessellations/notes", json!(null))?.as_u16(), 204);
    Ok(())
}

#[test]
fn api_keys_are_shown_once_and_revocable() -> Result<()> {
    let server = TestServer::start()?;
    let created = server.request(Method::POST, "/auth/keys", Some(&json!({ "name": "ci", "expires_in_days": 30 })), &[])?;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let key = created.body["key"].as_str().unwrap().to_string();
    assert!(key.starts_with("hxk_"));
    assert_eq!(server.request_as(Some(&key), Method::GET, "/auth/me", None, &[])?.body["login"], TEST_ADMIN_LOGIN);

    let list = server.request(Method::GET, "/auth/keys", None, &[])?;
    // Keys are hxk_<id>_<secret>. The id has no underscore but the secret can,
    // so the whole secret is everything after the second underscore.
    let secret = key.splitn(3, '_').nth(2).unwrap();
    assert!(secret.len() >= 20, "{}", key);
    assert!(!list.body.to_string().contains(secret), "secrets are never listed");
    let id = list.body["keys"][0]["id"].as_str().unwrap().to_string();

    // Another user can't revoke it; a tampered key doesn't work.
    let other = server.user_with_roles("bo", json!([]))?;
    assert_eq!(server.request_as(Some(&other), Method::DELETE, &format!("/auth/keys/{}", id), None, &[])?.status, StatusCode::NOT_FOUND);
    let tampered = format!("{}x", &key[..key.len() - 1]);
    assert_eq!(server.request_as(Some(&tampered), Method::GET, "/auth/me", None, &[])?.status, StatusCode::UNAUTHORIZED);

    assert_eq!(server.request(Method::DELETE, &format!("/auth/keys/{}", id), None, &[])?.status, StatusCode::NO_CONTENT);
    assert_eq!(server.request_as(Some(&key), Method::GET, "/auth/me", None, &[])?.status, StatusCode::UNAUTHORIZED);

    // Keys are stored hashed: the secret appears nowhere in the data directory.
    server.flush()?;
    let secret = key.rsplit('_').next().unwrap();
    for entry in walk(&server.data_dir()) {
        let bytes = std::fs::read(&entry)?;
        assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()), "{} contains the key", entry.display());
    }
    Ok(())
}

#[test]
fn idempotency_keys_are_per_user() -> Result<()> {
    let server = TestServer::start()?;
    let other = server.user_with_roles("bo", json!([{ "name": "writer", "permissions": ["notes"] }]))?;
    let body = json!({ "n": 1 });
    let key = [("Idempotency-Key", "same-key")];
    let first = server.request(Method::POST, "/notes", Some(&body), &key)?;
    let theirs = server.request_as(Some(&other), Method::POST, "/notes", Some(&body), &key)?;
    assert_eq!(theirs.status, StatusCode::CREATED);
    assert!(!theirs.replayed(), "another user's key doesn't replay");
    assert_ne!(first.body["id"], theirs.body["id"]);
    assert_eq!(server.count("notes")?, 2);
    Ok(())
}

#[test]
fn responses_carry_security_headers_and_no_cors() -> Result<()> {
    let server = TestServer::start()?;
    let res = server.request_as(None, Method::GET, "/health", None, &[("Origin", "https://evil.example")])?;
    for (name, value) in [
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        ("cache-control", "no-store"),
    ] {
        assert_eq!(res.headers.get(name).map(|v| v.to_str().unwrap()), Some(value), "{}", name);
    }
    assert!(res.headers["content-security-policy"].to_str()?.contains("frame-ancestors 'none'"));
    assert!(res.headers.get("access-control-allow-origin").is_none(), "no CORS: other sites can't read responses");
    Ok(())
}

#[test]
fn lattice_endpoints_need_a_fresh_signature() -> Result<()> {
    let server = TestServer::start()?;
    for path in ["/lattice/snapshot", "/lattice/catalog", "/lattice/changes?after=0", "/lattice/snapshot/users"] {
        let res = server.request_as(None, Method::GET, path, None, &[])?;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{}", path);
        // An admin session is not a lattice credential, and neither is the old static token header.
        let res = server.request(Method::GET, path, None, &[("x-hexdb-lattice-token", "anything")])?;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{}", path);
        let res = server.request_as(None, Method::GET, path, None, &[("x-hexdb-lattice-signature", "1.2.3")])?;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{}", path);
    }
    let res = server.request_as(None, Method::POST, "/lattice/revoke", Some(&json!({ "session_id": "x", "expires_at": 0 })), &[])?;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    Ok(())
}

#[test]
fn first_admin_password_is_generated_when_not_configured() -> Result<()> {
    let server = TestServer::start_with(TestOptions { no_admin_password: true, ..Default::default() })?;
    let file = server.data_dir().join("initial-admin-password.txt");
    let text = std::fs::read_to_string(&file)?;
    let password = text.lines().find_map(|l| l.trim().strip_prefix("password:")).unwrap().trim().to_string();
    assert!(password.len() >= 20, "{}", password);
    assert_eq!(login(&server, TEST_ADMIN_LOGIN, &password)?.status, StatusCode::OK);
    assert!(!server.log_tail(500).contains(&password) || server.log_tail(500).contains("only time it is shown"), "the password only goes to the console banner");
    let logs_ok = login(&server, TEST_ADMIN_LOGIN, &password)?;
    let token = logs_ok.body["token"].as_str().unwrap().to_string();
    let logs = server.request_as(Some(&token), Method::GET, "/logs?limit=1000", None, &[])?;
    assert!(!logs.body.to_string().contains(&password), "never in the in-memory log");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&file)?.permissions().mode() & 0o777, 0o600);
    }
    Ok(())
}

#[test]
fn nothing_is_stored_in_plaintext() -> Result<()> {
    let mut server = TestServer::start()?;
    let marker = "PLAINTEXT-CANARY-7d1f9c";
    server.insert("notes", &json!({ "secret": marker }))?;
    // In the WAL...
    for entry in walk(&server.data_dir()) {
        assert!(!std::fs::read(&entry)?.windows(marker.len()).any(|w| w == marker.as_bytes()), "{}", entry.display());
    }
    // ...and in SSTables after a flush and a restart.
    server.flush()?;
    server.restart()?;
    for entry in walk(&server.data_dir()) {
        assert!(!std::fs::read(&entry)?.windows(marker.len()).any(|w| w == marker.as_bytes()), "{}", entry.display());
    }
    assert_eq!(server.request(Method::POST, "/notes/_query", Some(&json!({})), &[])?.body["documents"][0]["secret"], marker);
    Ok(())
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn https_serves_the_api_and_carries_replication() -> Result<()> {
    let certificate = hexdb_tests::self_signed_certificate()?;
    let ports = [TestServer::free_port()?, TestServer::free_port()?];
    let options = |i: usize, ram: u64| TestOptions {
        lattice: Some(format!("tls-lattice-{}", ports[0])),
        discovery_port: Some(ports[i]),
        peers: vec![format!("127.0.0.1:{}", ports[1 - i])],
        ram_mb: Some(ram),
        discovery_interval_seconds: Some(1),
        tls: Some(certificate.clone()),
        ..Default::default()
    };
    let overseer = TestServer::start_with(options(0, 4096))?;
    assert!(overseer.url("/").starts_with("https://"));
    let id = overseer.insert("notes", &json!({ "over": "tls" }))?;

    // HSTS, and the session cookie is Secure.
    let res = overseer.request_as(None, Method::POST, "/auth/login", Some(&json!({ "login": TEST_ADMIN_LOGIN, "password": TEST_ADMIN_PASSWORD })), &[])?;
    assert!(res.headers["set-cookie"].to_str()?.contains("; Secure"));
    assert!(res.headers["strict-transport-security"].to_str()?.contains("max-age="));

    // Plain HTTP to the TLS port doesn't get an answer.
    let plain = overseer.url("/health").replace("https://", "http://");
    assert!(!reqwest::blocking::get(&plain).map(|r| r.status().is_success()).unwrap_or(false));

    // A replica follows the Overseer over HTTPS (signed requests, verified certificate).
    let replica = TestServer::start_with(options(1, 1024))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if replica.get_doc("notes", &id)?.is_some() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "replication over TLS:\n{}", replica.log_tail(40));
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Ok(())
}

#[test]
fn storage_keys_rotate_without_downtime() -> Result<()> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let old_key = hexdb_tests::TEST_ENCRYPTION_KEY.to_string();
    let new_key = format!("base64:{}", STANDARD.encode([42u8; 32]));
    let mut server = TestServer::start()?;
    let id = server.insert("notes", &json!({ "v": "before rotation" }))?;
    server.flush()?;
    server.stop()?;

    // The new key alone can't read the old data: the server refuses to start rather than lose it.
    let dir_options = |key: &str, previous: Vec<String>| TestOptions { encryption_key: Some(key.into()), previous_encryption_keys: previous, ..Default::default() };
    server.set_options(dir_options(&new_key, vec![]));
    let err = server.launch().unwrap_err().to_string();
    assert!(err.contains("previous_encryption_keys"), "{}", err);

    // With the old key listed as previous, everything reads; compaction re-encrypts.
    server.set_options(dir_options(&new_key, vec![old_key.clone()]));
    server.launch()?;
    assert_eq!(server.get_doc("notes", &id)?.unwrap()["v"], "before rotation");
    let id2 = server.insert("notes", &json!({ "v": "after rotation" }))?;
    let status = server.request(Method::GET, "/status", None, &[])?;
    assert!(status.body["storage"]["sstable_files_on_old_keys"].as_u64().unwrap() >= 1, "{}", status.body["storage"]);
    let compacted = server.request(Method::POST, "/compact", None, &[])?;
    assert_eq!(compacted.body["sstable_files_on_old_keys"], 0, "{}", compacted.body);
    server.flush()?;
    server.stop()?;

    // Now the old key can be dropped.
    server.set_options(dir_options(&new_key, vec![]));
    server.launch()?;
    assert_eq!(server.get_doc("notes", &id)?.unwrap()["v"], "before rotation");
    assert_eq!(server.get_doc("notes", &id2)?.unwrap()["v"], "after rotation");
    Ok(())
}
