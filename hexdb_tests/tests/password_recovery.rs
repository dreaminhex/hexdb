//! `hexdb reset-password`: a forgotten administrator password is replaced from
//! the server's machine, while it runs or at its next start.

use anyhow::Result;
use hexdb_tests::{TestServer, TEST_ADMIN_LOGIN, TEST_ADMIN_PASSWORD};
use reqwest::{blocking::Client, Method, StatusCode};
use serde_json::json;
use std::process::{Command, Output};

fn cli(server: &TestServer, args: &[&str]) -> Result<Output> {
    let config = server.dir().join("hexdb.toml");
    Ok(Command::new(hexdb_tests::cli_bin()?).arg("--config").arg(&config).args(args).current_dir(server.dir()).output()?)
}

fn sign_in(server: &TestServer, password: &str) -> Result<StatusCode> {
    let client: Client = hexdb_tests::http().build()?;
    let res = client.post(server.url("/auth/login")).json(&json!({ "login": TEST_ADMIN_LOGIN, "password": password })).send()?;
    Ok(res.status())
}

fn printed_password(output: &Output) -> Option<String> {
    String::from_utf8_lossy(&output.stdout).lines().find_map(|l| l.trim().strip_prefix("password:").map(|p| p.trim().to_string()))
}

#[test]
fn a_running_server_resets_the_admin_password() -> Result<()> {
    let server = TestServer::start()?;
    let out = cli(&server, &["reset-password"])?;
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let password = printed_password(&out).expect("the new password is printed");

    assert_eq!(sign_in(&server, TEST_ADMIN_PASSWORD)?, StatusCode::UNAUTHORIZED, "the old password no longer works");
    assert!(sign_in(&server, &password)?.is_success(), "the new one does");
    let old_session = server.request(Method::GET, "/auth/me", None, &[])?;
    assert_eq!(old_session.status, StatusCode::UNAUTHORIZED, "sessions from before the reset are signed out");
    assert!(!server.data_dir().join("reset-password.result").exists(), "the CLI removes the answer");

    let unknown = cli(&server, &["reset-password", "--login", "nobody"])?;
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("refused"), "{}", String::from_utf8_lossy(&unknown.stderr));
    Ok(())
}

#[test]
fn a_stopped_server_resets_at_its_next_start() -> Result<()> {
    let mut server = TestServer::start()?;
    server.stop()?;
    let out = cli(&server, &["reset-password", "--mfa-off"])?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("isn't running"), "{}", String::from_utf8_lossy(&out.stdout));

    // The test harness signs in with the old password, which no longer works after the reset.
    server.set_options(hexdb_tests::TestOptions { no_admin_password: true, ..Default::default() });
    server.launch()?;
    let password = printed_password(&out).expect("the new password is printed");
    // Failed sign-ins would count towards a lockout: wait for the server's answer first.
    let answer = server.data_dir().join("reset-password.result");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !answer.exists() {
        assert!(std::time::Instant::now() < deadline, "the server never answered");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert!(!server.data_dir().join("reset-password.request").exists(), "the request is used once");
    assert!(sign_in(&server, &password)?.is_success());
    Ok(())
}
