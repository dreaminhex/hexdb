//! Passwords that work once: the first administrator's, one an administrator
//! hands out, and one `hexdb reset-password` prints. The sign-in that uses it
//! must choose a new one, on any hex.

use anyhow::{bail, Result};
use hexdb_tests::{TestOptions, TestServer, TEST_ADMIN_LOGIN, TEST_ADMIN_PASSWORD};
use reqwest::Method;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const NEW_PASSWORD: &str = "A brand new passphrase 2026";

fn sign_in(server: &TestServer, login: &str, password: &str, new_password: Option<&str>) -> Result<(u16, Value)> {
    let mut body = json!({ "login": login, "password": password, "return_token": true });
    if let Some(new) = new_password {
        body["new_password"] = json!(new);
    }
    let res = server.request_as(None, Method::POST, "/auth/login", Some(&body), &[])?;
    Ok((res.status.as_u16(), res.body))
}

fn wait_until(timeout: Duration, what: &str, mut check: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if check()? {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {}", what);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
fn the_first_administrator_chooses_their_own_password() -> Result<()> {
    let server = TestServer::start_with(TestOptions { security_toml: "change_first_password = true\n".into(), ..Default::default() })?;

    // The configured password is right, but it only opens the door to choosing a new one.
    let (status, body) = sign_in(&server, TEST_ADMIN_LOGIN, TEST_ADMIN_PASSWORD, None)?;
    assert_eq!((status, body["error"]["code"].as_str()), (403, Some("password_change_required")), "{}", body);
    assert!(body.get("token").is_none());
    // A weak one, or the same one, is refused; a wrong current password still fails as always.
    assert_eq!(sign_in(&server, TEST_ADMIN_LOGIN, TEST_ADMIN_PASSWORD, Some("short"))?.0, 400);
    assert_eq!(sign_in(&server, TEST_ADMIN_LOGIN, TEST_ADMIN_PASSWORD, Some(TEST_ADMIN_PASSWORD))?.0, 400);
    assert_eq!(sign_in(&server, TEST_ADMIN_LOGIN, "not the password", Some(NEW_PASSWORD))?.0, 401);

    let (status, body) = sign_in(&server, TEST_ADMIN_LOGIN, TEST_ADMIN_PASSWORD, Some(NEW_PASSWORD))?;
    assert_eq!(status, 200, "{}", body);
    let token = body["token"].as_str().unwrap().to_string();
    assert_eq!(body["user"]["login"], TEST_ADMIN_LOGIN);

    // The old password is gone; the new one signs in normally.
    assert_eq!(sign_in(&server, TEST_ADMIN_LOGIN, TEST_ADMIN_PASSWORD, None)?.0, 401);
    assert_eq!(sign_in(&server, TEST_ADMIN_LOGIN, NEW_PASSWORD, None)?.0, 200);
    let me = server.request_as(Some(&token), Method::GET, &format!("/users/{}", TEST_ADMIN_LOGIN), None, &[])?;
    assert_eq!(me.body["password_change_required"], false, "{}", me.body);
    let audit = server.request_as(Some(&token), Method::GET, "/audit?action=auth.password_change", None, &[])?.body;
    assert_eq!(audit["events"].as_array().map(Vec::len), Some(1), "{}", audit);
    Ok(())
}

#[test]
fn a_temporary_password_is_changed_at_sign_in_on_a_replica() -> Result<()> {
    const TEMPORARY: &str = "Temporary passphrase 2026";
    let ports = [TestServer::free_port()?, TestServer::free_port()?];
    let options = |i: usize, ram: u64| TestOptions {
        lattice: Some(format!("password-lattice-{}", ports[0])),
        discovery_port: Some(ports[i]),
        peers: vec![format!("127.0.0.1:{}", ports[1 - i])],
        ram_mb: Some(ram),
        discovery_interval_seconds: Some(1),
        ..Default::default()
    };
    let overseer = TestServer::start_with(options(0, 4096))?;
    let replica = TestServer::start_with(options(1, 1024))?;

    let user = json!({ "login": "ada", "password": TEMPORARY, "email_address": "ada@example.com", "roles": [{ "name": "reader", "tessellations": ["*"] }], "password_change_required": true });
    let created = overseer.request(Method::POST, "/users", Some(&user), &[])?;
    assert_eq!(created.status, 201, "{}", created.body);
    assert_eq!(created.body["password_change_required"], true, "{}", created.body);
    wait_until(Duration::from_secs(30), "the user on the replica", || Ok(sign_in(&replica, "ada", TEMPORARY, None)?.0 == 403))?;

    // On the replica: the new password goes to the Overseer, which stores it.
    let (status, body) = sign_in(&replica, "ada", TEMPORARY, Some(NEW_PASSWORD))?;
    assert_eq!(status, 200, "{}", body);
    assert_eq!(sign_in(&overseer, "ada", NEW_PASSWORD, None)?.0, 200);
    assert_eq!(sign_in(&overseer, "ada", TEMPORARY, None)?.0, 401);
    wait_until(Duration::from_secs(30), "the new password on the replica", || Ok(sign_in(&replica, "ada", NEW_PASSWORD, None)?.0 == 200))?;
    let ada = overseer.request(Method::GET, "/users/ada", None, &[])?;
    assert_eq!(ada.body["password_change_required"], false, "{}", ada.body);

    // An administrator setting a new password makes it the user's own, unless told otherwise.
    let reset = overseer.request(Method::PATCH, "/users/ada", Some(&json!({ "password": TEMPORARY, "password_change_required": true })), &[])?;
    assert_eq!(reset.body["password_change_required"], true, "{}", reset.body);
    assert_eq!(sign_in(&overseer, "ada", TEMPORARY, None)?.0, 403);
    let own = overseer.request(Method::PATCH, "/users/ada", Some(&json!({ "password": NEW_PASSWORD })), &[])?;
    assert_eq!(own.body["password_change_required"], false, "{}", own.body);
    assert_eq!(sign_in(&overseer, "ada", NEW_PASSWORD, None)?.0, 200);
    Ok(())
}
