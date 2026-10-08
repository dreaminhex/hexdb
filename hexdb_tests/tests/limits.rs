//! Connection limits and timeouts.

use anyhow::Result;
use hexdb_tests::{TestOptions, TestServer};
use reqwest::Method;
use serde_json::json;
use std::{
    io::{Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

fn limited() -> Result<TestServer> {
    TestServer::start_with(TestOptions {
        extra_toml: "\n[limits]\nmax_connections_per_client = 4\nheader_timeout_seconds = 2\nrequest_timeout_seconds = 2\n".into(),
        ..Default::default()
    })
}

fn address(server: &TestServer) -> String {
    server.url("").trim_start_matches("http://").to_string()
}

/// True if the server closes the connection (EOF or reset) within `within`.
fn closed_within(stream: &mut TcpStream, within: Duration) -> bool {
    stream.set_read_timeout(Some(within)).unwrap();
    let mut buf = [0u8; 64];
    match stream.read(&mut buf) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
    }
}

#[test]
fn connections_per_client_are_capped_and_idle_ones_time_out() -> Result<()> {
    let server = limited()?;
    let addr = address(&server);

    // Open idle connections until the server starts refusing them.
    let mut idle = Vec::new();
    let mut refused = false;
    for _ in 0..6 {
        let mut stream = TcpStream::connect(&addr)?;
        if closed_within(&mut stream, Duration::from_millis(300)) {
            refused = true;
            break;
        }
        idle.push(stream);
    }
    assert!(refused, "a fifth connection from one address is closed");
    assert!(idle.len() <= 4);

    // Connections that never send a request are closed after the header timeout.
    let started = Instant::now();
    for stream in &mut idle {
        assert!(closed_within(stream, Duration::from_secs(5)), "idle connection closed");
    }
    assert!(started.elapsed() < Duration::from_secs(5));

    // Afterwards the client can connect again.
    assert!(server.request(Method::GET, "/health", None, &[])?.status.is_success());
    Ok(())
}

#[test]
fn slow_request_bodies_time_out() -> Result<()> {
    let server = limited()?;
    let mut stream = TcpStream::connect(address(&server))?;
    let body = json!({ "text": "x".repeat(50) }).to_string();
    // Headers promise the whole body, but only half of it is sent.
    write!(
        stream,
        "POST /notes HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        server.token(),
        body.len(),
        &body[..body.len() / 2]
    )?;
    let started = Instant::now();
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut response = String::new();
    let mut buf = [0u8; 4096];
    while !response.contains("\r\n\r\n") {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        response.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(response.starts_with("HTTP/1.1 503"), "{}", response);
    assert!(started.elapsed() < Duration::from_secs(5));

    // Normal requests are unaffected.
    server.insert("notes", &json!({ "fast": true }))?;
    Ok(())
}

#[test]
fn long_polls_outlive_the_request_timeout() -> Result<()> {
    let server = limited()?;
    let started = Instant::now();
    let res = server.request(Method::GET, "/changes?wait=3", None, &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    assert!(started.elapsed() >= Duration::from_secs(3), "the long poll waited its full time");
    Ok(())
}

/// Fail sign-in `n` times as `login`, claiming to come from `forwarded`.
fn fail_logins(server: &TestServer, login: &str, forwarded: &str, n: usize) -> Result<Vec<u16>> {
    let mut statuses = Vec::new();
    for _ in 0..n {
        let res = server.request_as(
            None,
            Method::POST,
            "/auth/login",
            Some(&json!({ "login": login, "password": "wrong password here" })),
            &[("x-forwarded-for", forwarded)],
        )?;
        statuses.push(res.status.as_u16());
    }
    Ok(statuses)
}

#[test]
fn forwarded_addresses_are_trusted_only_from_configured_proxies() -> Result<()> {
    // Trusted: each forwarded address is throttled on its own.
    let server = TestServer::start_with(TestOptions { network_toml: "trusted_proxies = [\"127.0.0.1\"]".into(), ..Default::default() })?;
    assert_eq!(fail_logins(&server, "ghost-a", "198.51.100.1", 6)?.last(), Some(&429));
    assert_eq!(fail_logins(&server, "ghost-b", "198.51.100.2", 1)?, vec![401], "another client isn't blocked");
    assert_eq!(fail_logins(&server, "ghost-c", "198.51.100.1", 1)?, vec![429], "the throttled client stays blocked");

    // Not trusted: the header is ignored, so every attempt is the same client.
    let server = TestServer::start()?;
    assert_eq!(fail_logins(&server, "ghost-a", "198.51.100.1", 6)?.last(), Some(&429));
    assert_eq!(fail_logins(&server, "ghost-b", "198.51.100.2", 1)?, vec![429], "a spoofed header doesn't escape the throttle");
    Ok(())
}
