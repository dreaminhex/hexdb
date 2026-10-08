//! Security across a lattice: shared sign-in throttling, audit events from
//! replicas, and rotating the lattice secret one hex at a time.

use anyhow::{bail, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use hexdb_tests::{TestOptions, TestServer};
use reqwest::Method;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

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

fn secret(byte: u8) -> String {
    format!("base64:{}", STANDARD.encode([byte; 32]))
}

/// Options for a two-hex lattice; `network` adds lines to `[network]`.
fn pair_options(ports: [u16; 2], i: usize, ram_mb: u64, network: &str) -> TestOptions {
    TestOptions {
        lattice: Some(format!("security-lattice-{}", ports[0])),
        discovery_port: Some(ports[i]),
        peers: vec![format!("127.0.0.1:{}", ports[1 - i])],
        ram_mb: Some(ram_mb),
        discovery_interval_seconds: Some(1),
        network_toml: network.into(),
        ..Default::default()
    }
}

fn replicated(replica: &TestServer, tess: &str, id: &str) -> Result<()> {
    wait_until(Duration::from_secs(30), "replication", || Ok(replica.get_doc(tess, id)?.is_some()))
        .map_err(|e| anyhow::anyhow!("{}:\n{}", e, replica.log_tail(40)))
}

/// A failed sign-in from a client address of its own (through a trusted
/// proxy header), so the harness's own sign-ins from 127.0.0.1 don't count.
fn fail_login(server: &TestServer, login: &str) -> Result<u16> {
    let res = server.request_as(
        None,
        Method::POST,
        "/auth/login",
        Some(&json!({ "login": login, "password": "definitely wrong" })),
        &[("x-forwarded-for", "203.0.113.7")],
    )?;
    Ok(res.status.as_u16())
}

#[test]
fn sign_in_throttling_and_audit_events_span_the_lattice() -> Result<()> {
    let ports = [TestServer::free_port()?, TestServer::free_port()?];
    let proxies = "trusted_proxies = [\"127.0.0.1\"]";
    let overseer = TestServer::start_with(pair_options(ports, 0, 4096, proxies))?;
    let replica = TestServer::start_with(pair_options(ports, 1, 1024, proxies))?;
    let id = overseer.insert("notes", &json!({ "n": 1 }))?;
    replicated(&replica, "notes", &id)?;

    // Five failures on the replica (the default limit)...
    for attempt in 1..=5 {
        let status = fail_login(&replica, "target-user")?;
        assert_eq!(status, 401, "attempt {} was refused early:
{}", attempt, replica.log_tail(30));
    }
    // ...block that login on the Overseer too, once the record arrives.
    wait_until(Duration::from_secs(10), "the throttle to reach the Overseer", || Ok(fail_login(&overseer, "target-user")? == 429))?;

    // The replica's failures reach the Overseer's audit trail, recorded by the
    // replica (forwarding is asynchronous, so wait for all five).
    let replica_name = replica.request(Method::GET, "/status", None, &[])?.body["name"].as_str().unwrap_or_default().to_string();
    let from_replica = || -> Result<(usize, Value)> {
        let events: Value = overseer.request(Method::GET, "/audit?action=auth.login&target=target-user&outcome=failed", None, &[])?.body;
        let count = events["events"].as_array().map_or(0, |a| a.iter().filter(|e| e["hex"] == replica_name.as_str()).count());
        Ok((count, events))
    };
    wait_until(Duration::from_secs(10), "the replica's audit events", || Ok(from_replica()?.0 >= 5))
        .map_err(|e| anyhow::anyhow!("{}: {}", e, from_replica().map(|r| r.1).unwrap_or_default()))?;
    let (count, events) = from_replica()?;
    assert_eq!(count, 5, "{}", events);
    Ok(())
}

#[test]
fn the_lattice_secret_rotates_one_hex_at_a_time() -> Result<()> {
    let (old, new) = (secret(7), secret(8));
    let ports = [TestServer::free_port()?, TestServer::free_port()?];
    let only_old = format!("lattice_secret = \"{}\"", old);
    let rotated = format!("lattice_secret = \"{}\"\nprevious_lattice_secrets = [\"{}\"]", new, old);
    let only_new = format!("lattice_secret = \"{}\"", new);

    // Both hexes on the old secret.
    let mut overseer = TestServer::start_with(pair_options(ports, 0, 4096, &only_old))?;
    let mut replica = TestServer::start_with(pair_options(ports, 1, 1024, &only_old))?;
    let first = overseer.insert("notes", &json!({ "step": 1 }))?;
    replicated(&replica, "notes", &first)?;
    let key = overseer.user_with_roles("svc", json!([{ "name": "reader", "tessellations": ["*"] }]))?;

    // Step 1: the replica moves to the new secret, keeping the old one. It
    // still follows the Overseer, which knows only the old secret.
    replica.stop()?;
    replica.set_options(pair_options(ports, 1, 1024, &rotated));
    replica.launch()?;
    let second = overseer.insert("notes", &json!({ "step": 2 }))?;
    replicated(&replica, "notes", &second)?;

    // Step 2: the Overseer moves too. Sessions and API keys made under the old
    // secret keep working.
    overseer.stop()?;
    overseer.set_options(pair_options(ports, 0, 4096, &rotated));
    overseer.launch()?;
    assert_eq!(overseer.request_as(Some(&key), Method::GET, "/notes", None, &[])?.status, 200, "API key after rotation");
    let third = overseer.insert("notes", &json!({ "step": 3 }))?;
    replicated(&replica, "notes", &third)?;

    // Step 3: the old secret is dropped everywhere.
    for (server, i, ram) in [(&mut replica, 1, 1024), (&mut overseer, 0, 4096)] {
        server.stop()?;
        server.set_options(pair_options(ports, i, ram, &only_new));
        server.launch()?;
    }
    assert_eq!(overseer.request_as(Some(&key), Method::GET, "/notes", None, &[])?.status, 200, "the key was re-hashed with the new secret");
    let fourth = overseer.insert("notes", &json!({ "step": 4 }))?;
    replicated(&replica, "notes", &fourth)?;

    // A hex that only knows the old secret is now a stranger.
    let stranger = TestServer::start_with(TestOptions {
        peers: vec![format!("127.0.0.1:{}", ports[0])],
        ..pair_options([TestServer::free_port()?, ports[0]], 0, 512, &only_old)
    })?;
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(stranger.request(Method::GET, "/status", None, &[])?.body["hex_type"], "Overseer", "it found no lattice to join");
    Ok(())
}
