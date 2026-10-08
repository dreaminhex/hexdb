//! Multi-factor authentication (TOTP and backup codes).

use anyhow::Result;
use hexdb_core::mfa::code_for;
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::{json, Value};

const PASSWORD: &str = "Sturdy test passphrase 2026";

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn sign_in(server: &TestServer, code: Option<&str>) -> Result<(u16, Value)> {
    let mut body = json!({ "login": "ada", "password": PASSWORD, "return_token": true });
    if let Some(code) = code {
        body["code"] = json!(code);
    }
    let res = server.request_as(None, Method::POST, "/auth/login", Some(&body), &[])?;
    Ok((res.status.as_u16(), res.body))
}

#[test]
fn totp_and_backup_codes_guard_sign_in() -> Result<()> {
    let server = TestServer::start()?;
    let created = server.request(
        Method::POST,
        "/users",
        Some(&json!({ "login": "ada", "password": PASSWORD, "email_address": "ada@example.com", "roles": [{ "name": "reader", "tessellations": ["*"] }] })),
        &[],
    )?;
    assert_eq!(created.status, 201, "{}", created.body);
    let (_, body) = sign_in(&server, None)?;
    let session = body["token"].as_str().unwrap().to_string();
    let as_ada = |method: Method, path: &str, body: Option<&Value>| server.request_as(Some(&session), method, path, body, &[]);

    // Enrolment needs the password, then a valid code.
    assert_eq!(as_ada(Method::POST, "/auth/mfa/setup", Some(&json!({ "password": "wrong password!!" })))?.status, 403);
    let setup = as_ada(Method::POST, "/auth/mfa/setup", Some(&json!({ "password": PASSWORD })))?;
    assert_eq!(setup.status, 200, "{}", setup.body);
    let secret = setup.body["secret"].as_str().unwrap().to_string();
    assert!(setup.body["otpauth_uri"].as_str().unwrap().starts_with("otpauth://totp/"));
    assert_eq!(as_ada(Method::POST, "/auth/mfa/enable", Some(&json!({ "code": "000000" })))?.status, 400);
    let enabled = as_ada(Method::POST, "/auth/mfa/enable", Some(&json!({ "code": code_for(&secret, now()).unwrap() })))?;
    assert_eq!(enabled.status, 200, "{}", enabled.body);
    let backup: Vec<String> = serde_json::from_value(enabled.body["backup_codes"].clone())?;
    assert_eq!(backup.len(), 10);

    // Sessions from before MFA was on are signed out.
    assert_eq!(as_ada(Method::GET, "/auth/me", None)?.status, 401);

    // The password alone isn't enough; a wrong code isn't either.
    let (status, body) = sign_in(&server, None)?;
    assert_eq!((status, body["error"]["code"].as_str()), (401, Some("mfa_required")));
    assert_eq!(sign_in(&server, Some("123456"))?.0, 401);

    // The next step's code works (the current one was used to enable), once.
    let next = code_for(&secret, now() + 30).unwrap();
    let (status, body) = sign_in(&server, Some(&next))?;
    assert_eq!(status, 200, "{}", body);
    assert_eq!(sign_in(&server, Some(&next))?.0, 401, "a code can't be replayed");

    // A backup code works once.
    assert_eq!(sign_in(&server, Some(&backup[0]))?.0, 200);
    assert_eq!(sign_in(&server, Some(&backup[0]))?.0, 401);
    let token = sign_in(&server, Some(&backup[1]))?.1["token"].as_str().unwrap().to_string();
    let status = server.request_as(Some(&token), Method::GET, "/auth/mfa", None, &[])?;
    assert_eq!(status.body["backup_codes_left"], 8);
    assert_eq!(status.body["enabled"], true);

    // Administrators can reset MFA, but not turn it on for someone.
    assert_eq!(server.request(Method::PATCH, "/users/ada", Some(&json!({ "use_mfa": false })), &[])?.status, 200);
    assert_eq!(sign_in(&server, None)?.0, 200);
    assert_eq!(server.request(Method::PATCH, "/users/ada", Some(&json!({ "use_mfa": true })), &[])?.status, 400);

    // Enrolment is audited.
    let audit = server.request(Method::GET, "/audit?action=auth.mfa_enable", None, &[])?;
    assert_eq!(audit.body["events"][0]["actor"], "ada");
    Ok(())
}
