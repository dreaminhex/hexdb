//! Cross-origin requests: only origins in `cors.allowed_origins` get CORS
//! headers, preflights are answered before authentication, and credentials
//! (the session cookie) are never allowed across origins.

use anyhow::Result;
use hexdb_tests::{TestServer, TEST_ADMIN_LOGIN, TEST_ADMIN_PASSWORD};
use reqwest::Method;
use serde_json::json;

const APP: &str = "https://app.example.com";

fn allow_origins(server: &TestServer, origins: &str) -> Result<()> {
    let res = server.request(Method::PUT, "/config", Some(&json!({ "changes": { "cors.allowed_origins": origins } })), &[])?;
    assert_eq!(res.status, 200, "{}", res.body);
    Ok(())
}

fn preflight(server: &TestServer, origin: &str) -> Result<reqwest::blocking::Response> {
    Ok(hexdb_tests::http()
        .build()?
        .request(Method::OPTIONS, server.url("/notes"))
        .header("origin", origin)
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "authorization, content-type")
        .send()?)
}

#[test]
fn listed_origins_get_cors_headers_and_others_do_not() -> Result<()> {
    let server = TestServer::start()?;
    server.insert("notes", &json!({ "n": 1 }))?;

    // Off by default: no headers, and preflights are refused.
    let res = server.request(Method::GET, "/tessellations", None, &[("origin", APP)])?;
    assert_eq!(res.status, 200);
    assert!(res.headers.get("access-control-allow-origin").is_none());
    assert_eq!(preflight(&server, APP)?.status(), 403);

    // Bad values are refused.
    let res = server.request(Method::PUT, "/config", Some(&json!({ "changes": { "cors.allowed_origins": "app.example.com/path" } })), &[])?;
    assert_eq!(res.status, 400, "{}", res.body);

    allow_origins(&server, "https://App.Example.com/, http://localhost:5173")?;
    let pre = preflight(&server, APP)?;
    assert_eq!(pre.status(), 204, "answered without credentials");
    assert_eq!(pre.headers()["access-control-allow-origin"], APP);
    assert!(pre.headers()["access-control-allow-methods"].to_str()?.contains("PATCH"));
    assert_eq!(pre.headers()["access-control-allow-headers"], "authorization, content-type");
    assert!(pre.headers().get("access-control-allow-credentials").is_none(), "never with credentials");

    let res = server.request(Method::GET, "/tessellations", None, &[("origin", APP)])?;
    assert_eq!(res.headers["access-control-allow-origin"], APP);
    assert!(res.headers["access-control-expose-headers"].to_str()?.contains("etag"));
    assert!(res.headers.get_all("vary").iter().any(|v| v == "origin"));
    assert!(res.headers.get("access-control-allow-credentials").is_none());

    // Another origin: nothing.
    let res = server.request(Method::GET, "/tessellations", None, &[("origin", "https://evil.example")])?;
    assert!(res.headers.get("access-control-allow-origin").is_none());
    assert_eq!(preflight(&server, "https://evil.example")?.status(), 403);

    // A listed origin may sign in (for a token); others still may not.
    let anon = hexdb_tests::http().build()?;
    let login = json!({ "login": TEST_ADMIN_LOGIN, "password": TEST_ADMIN_PASSWORD, "return_token": true });
    let res = anon.post(server.url("/auth/login")).header("origin", APP).json(&login).send()?;
    assert_eq!(res.status(), 200);
    let res = anon.post(server.url("/auth/login")).header("origin", "https://evil.example").json(&login).send()?;
    assert_eq!(res.status(), 403);

    // `*`: any origin, with the literal * (no Vary needed), still no credentials and no sign-in.
    allow_origins(&server, "*")?;
    let res = server.request(Method::GET, "/tessellations", None, &[("origin", "https://anyone.example")])?;
    assert_eq!(res.headers["access-control-allow-origin"], "*");
    let res = anon.post(server.url("/auth/login")).header("origin", "https://anyone.example").json(&login).send()?;
    assert_eq!(res.status(), 403, "* doesn't open sign-in");
    Ok(())
}

#[test]
fn the_session_cookie_still_cant_make_changes_across_origins() -> Result<()> {
    let server = TestServer::start()?;
    allow_origins(&server, APP)?;
    let client = hexdb_tests::http().build()?;
    let res = client.post(server.url("/auth/login")).json(&json!({ "login": TEST_ADMIN_LOGIN, "password": TEST_ADMIN_PASSWORD })).send()?;
    let cookie = res.headers()["set-cookie"].to_str()?.split(';').next().unwrap().to_string();
    let res = client
        .post(server.url("/notes"))
        .header("cookie", cookie)
        .header("origin", APP)
        .json(&json!({ "n": 1 }))
        .send()?;
    assert_eq!(res.status(), 403, "CSRF protection is unchanged");
    Ok(())
}
