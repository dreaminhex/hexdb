//! Durable change history, resuming replication after an Overseer restart,
//! synchronous acknowledgements, quorum, and exact replica lag.

use anyhow::{bail, Result};
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

fn pair(ports: [u16; 2], i: usize, ram_mb: u64, extra: &str) -> TestOptions {
    TestOptions {
        lattice: Some(format!("replication-{}", ports[0])),
        discovery_port: Some(ports[i]),
        peers: vec![format!("127.0.0.1:{}", ports[1 - i])],
        ram_mb: Some(ram_mb),
        discovery_interval_seconds: Some(1),
        extra_toml: extra.into(),
        ..Default::default()
    }
}

fn replication(server: &TestServer) -> Result<Value> {
    Ok(server.request(Method::GET, "/status", None, &[])?.body["replication"].clone())
}

fn has(server: &TestServer, id: &str) -> Result<bool> {
    Ok(server.get_doc("notes", id)?.is_some())
}

#[test]
fn the_change_feed_survives_restarts() -> Result<()> {
    let mut server = TestServer::start()?;
    let ids: Vec<String> = (0..3).map(|i| server.insert("notes", &json!({ "n": i }))).collect::<Result<_>>()?;
    server.restart()?;

    // The in-memory feed starts empty after a restart; older changes come from disk.
    let res = server.request(Method::GET, "/changes?after=0&limit=100", None, &[])?;
    assert_eq!(res.status, 200, "{}", res.body);
    let seen: Vec<&str> = res.body["changes"].as_array().unwrap().iter().filter_map(|c| c["id"].as_str()).collect();
    for id in &ids {
        assert!(seen.contains(&id.as_str()), "{} in {:?}", id, seen);
    }
    assert_eq!(res.body["changes"][0]["op"], "put");
    assert!(res.body["changes"][0]["document"]["n"].is_number());

    // Without a change history, positions before the restart are gone.
    let mut bare = TestServer::start_with(TestOptions { storage_toml: "change_history_hours = 0".into(), ..Default::default() })?;
    bare.insert("notes", &json!({ "n": 1 }))?;
    bare.restart()?;
    let res = bare.request(Method::GET, "/changes?after=0", None, &[])?;
    assert_eq!(res.status, 410, "{}", res.body);
    assert_eq!(res.error_code(), Some("history_expired"));
    Ok(())
}

#[test]
fn replicas_resume_after_an_overseer_restart_without_a_full_sync() -> Result<()> {
    let ports = [TestServer::free_port()?, TestServer::free_port()?];
    let mut overseer = TestServer::start_with(pair(ports, 0, 4096, ""))?;
    let replica = TestServer::start_with(pair(ports, 1, 1024, ""))?;
    let first = overseer.insert("notes", &json!({ "n": 1 }))?;
    wait_until(Duration::from_secs(30), "the first sync", || has(&replica, &first))?;
    let synced_at = replication(&replica)?["last_sync"].clone();
    assert!(synced_at.is_string(), "{}", replication(&replica)?);

    // While the Overseer is down and after it restarts, writes happen; the
    // replica picks them up from its cursor.
    overseer.restart()?;
    let ids: Vec<String> = (0..20).map(|i| overseer.insert("notes", &json!({ "n": i }))).collect::<Result<_>>()?;
    wait_until(Duration::from_secs(30), "resumed streaming", || has(&replica, ids.last().unwrap()))?;
    for id in &ids {
        assert!(has(&replica, id)?);
    }
    assert_eq!(replication(&replica)?["last_sync"], synced_at, "no second full sync");

    // The Overseer knows exactly where the replica is.
    wait_until(Duration::from_secs(10), "zero lag on the Overseer's view", || {
        let status = overseer.request(Method::GET, "/status", None, &[])?.body;
        let hexes = status["network"]["lattice"]["hexes"].as_array().cloned().unwrap_or_default();
        Ok(hexes.iter().any(|h| h["is_self"] == false && h["lag"] == 0))
    })?;
    Ok(())
}

#[test]
fn min_acks_holds_writes_until_a_replica_has_them() -> Result<()> {
    let ports = [TestServer::free_port()?, TestServer::free_port()?];
    let extra = "\n[replication]\nmin_acks = 1\nack_timeout_ms = 1500\n";
    let overseer = TestServer::start_with(pair(ports, 0, 4096, extra))?;

    // No replica yet: the write is kept, but the client is told it isn't replicated.
    let res = overseer.request(Method::POST, "/notes", Some(&json!({ "n": 1 })), &[])?;
    assert_eq!(res.status, 503, "{}", res.body);
    assert_eq!(res.error_code(), Some("replication_timeout"));
    assert_eq!(overseer.count("notes")?, 1, "the write itself committed");

    // With a replica following, writes are acknowledged once it has them.
    let replica = TestServer::start_with(pair(ports, 1, 1024, extra))?;
    wait_until(Duration::from_secs(30), "the replica to stream", || Ok(replication(&replica)?["state"] == "streaming"))?;
    for i in 0..5 {
        let started = Instant::now();
        let id = overseer.insert("notes", &json!({ "n": i }))?;
        assert!(started.elapsed() < Duration::from_millis(1500));
        assert!(has(&replica, &id)?, "acknowledged means the replica has it");
    }
    Ok(())
}

#[test]
fn an_overseer_without_quorum_refuses_writes() -> Result<()> {
    let ports = [TestServer::free_port()?, TestServer::free_port()?];
    let extra = "\n[replication]\nquorum = 2\n";
    // Set up users first, then require a quorum of two.
    let mut overseer = TestServer::start_with(pair(ports, 0, 4096, ""))?;
    overseer.stop()?;
    overseer.set_options(pair(ports, 0, 4096, extra));
    overseer.launch()?;
    let res = overseer.request(Method::POST, "/notes", Some(&json!({ "n": 1 })), &[])?;
    assert_eq!(res.status, 503, "{}", res.body);
    assert_eq!(res.error_code(), Some("no_quorum"));

    let mut replica = TestServer::start_with(pair(ports, 1, 1024, extra))?;
    wait_until(Duration::from_secs(20), "quorum", || Ok(overseer.request(Method::POST, "/notes", Some(&json!({ "n": 2 })), &[])?.status == 201))?;

    // Losing the other hex loses the quorum again.
    replica.kill();
    wait_until(Duration::from_secs(30), "quorum loss", || Ok(overseer.request(Method::POST, "/notes", Some(&json!({ "n": 3 })), &[])?.status == 503))?;
    Ok(())
}

#[test]
fn schemas_and_indexes_reach_every_hex_promptly() -> Result<()> {
    let ports = [TestServer::free_port()?, TestServer::free_port()?];
    let overseer = TestServer::start_with(pair(ports, 0, 4096, ""))?;
    let replica = TestServer::start_with(pair(ports, 1, 1024, ""))?;
    let first = overseer.insert("customers", &json!({ "email": "ada@example.com", "name": "Ada" }))?;
    wait_until(Duration::from_secs(30), "the first sync", || Ok(replica.get_doc("customers", &first)?.is_some()))?;

    // A schema and an index registered on the Overseer...
    let schema = json!({ "fields": { "email": { "type": "string", "required": true }, "name": { "type": "string" } }, "additional_fields": true });
    assert_eq!(overseer.request(Method::POST, "/tessellations/customers/schemas", Some(&schema), &[])?.status.as_u16(), 201);
    assert_eq!(overseer.request(Method::POST, "/tessellations/customers/indexes", Some(&json!({ "fields": ["email"], "unique": true })), &[])?.status.as_u16(), 201);

    // ...reach the replica well within the periodic catalog check (15 s).
    let started = Instant::now();
    wait_until(Duration::from_secs(10), "the schema and index on the replica", || {
        let schemas = replica.request(Method::GET, "/tessellations/customers/schemas", None, &[])?.body;
        let indexes = replica.request(Method::GET, "/tessellations/customers/indexes", None, &[])?.body;
        let has_index = indexes.as_array().or_else(|| indexes["indexes"].as_array()).is_some_and(|l| l.iter().any(|i| i["name"] == "email"));
        Ok(schemas["current"] == 1 && has_index)
    })?;
    assert!(started.elapsed() < Duration::from_secs(10));

    // The replica's queries use the index; writes through it obey the schema
    // (forwarded to the Overseer, which validates them).
    let q = replica.request(Method::POST, "/customers/_query", Some(&json!({ "filter": { "email": "ada@example.com" } })), &[])?;
    assert_eq!(q.body["plan"]["indexes"], json!(["email"]), "{}", q.body);
    let bad = replica.request(Method::POST, "/customers", Some(&json!({ "name": "no email" })), &[])?;
    assert_eq!(bad.status.as_u16(), 422, "{}", bad.body);
    assert_eq!(bad.body["error"]["code"], "schema_violation");
    let good = replica.request(Method::POST, "/customers", Some(&json!({ "email": "grace@example.com" })), &[])?;
    assert_eq!(good.status.as_u16(), 201, "{}", good.body);
    assert_eq!(good.body["_schema"], 1, "stamped with the lattice's schema version: {}", good.body);
    Ok(())
}
