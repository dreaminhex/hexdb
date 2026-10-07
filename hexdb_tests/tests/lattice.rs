//! Lattice tests: several real hexes discover each other, elect one Overseer,
//! fail over when it dies, and don't flap when it comes back.

use anyhow::{bail, Result};
use hexdb_tests::{TestOptions, TestServer};
use reqwest::Method;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Poll until `check` returns true, or fail after `timeout`.
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

fn status(server: &TestServer) -> Result<Value> {
    Ok(server.request(Method::GET, "/status", None, &[])?.body)
}

fn role(server: &TestServer) -> Result<String> {
    Ok(status(server)?["hex_type"].as_str().unwrap_or_default().to_string())
}

/// Number of hexes this server currently sees as active (including itself).
fn active_hexes(server: &TestServer) -> Result<usize> {
    let s = status(server)?;
    Ok(s["network"]["lattice"]["hexes"]
        .as_array()
        .map(|hexes| hexes.iter().filter(|h| h["status"] == "active").count())
        .unwrap_or(0))
}

struct Cluster {
    lattice: String,
    ports: Vec<u16>,
}

impl Cluster {
    fn new(size: usize) -> Result<Self> {
        let ports = (0..size).map(|_| TestServer::free_port()).collect::<Result<Vec<_>>>()?;
        Ok(Cluster { lattice: format!("test-lattice-{}", ports[0]), ports })
    }

    /// Options for node `i`: its discovery port, with every other node as a seed.
    fn options(&self, i: usize, ram_mb: u64, role: &str) -> TestOptions {
        TestOptions {
            lattice: Some(self.lattice.clone()),
            discovery_port: Some(self.ports[i]),
            peers: self
                .ports
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, p)| format!("127.0.0.1:{}", p))
                .collect(),
            ram_mb: Some(ram_mb),
            role: Some(role.into()),
            discovery_interval_seconds: Some(1),
            ..Default::default()
        }
    }
}

#[test]
fn hexes_elect_one_overseer_fail_over_and_do_not_flap() -> Result<()> {
    let cluster = Cluster::new(3)?;
    let mut big = TestServer::start_with(cluster.options(0, 4096, "auto"))?;
    let medium = TestServer::start_with(cluster.options(1, 2048, "auto"))?;
    let replicant = TestServer::start_with(cluster.options(2, 8192, "replicant"))?;
    let all = [&big, &medium, &replicant];

    // Everyone finds everyone.
    wait_until(Duration::from_secs(20), "all hexes to see each other", || {
        Ok(all.iter().map(|s| active_hexes(s)).collect::<Result<Vec<_>>>()?.iter().all(|n| *n == 3))
    })?;

    // The first hex led alone and keeps leading; the replicant never leads even with the most RAM.
    assert_eq!(role(&big)?, "Overseer");
    assert_eq!(role(&medium)?, "Harvester");
    assert_eq!(role(&replicant)?, "Replicant");

    // Every hex agrees on who leads.
    for server in all {
        let s = status(server)?;
        let overseers: Vec<&str> = s["network"]["lattice"]["hexes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|h| h["role"] == "Overseer")
            .map(|h| h["name"].as_str().unwrap())
            .collect();
        assert_eq!(overseers.len(), 1, "{:?}", overseers);
    }

    // Kill the Overseer: the medium hex takes over (the replicant still never leads).
    big.kill();
    wait_until(Duration::from_secs(20), "failover to the medium hex", || Ok(role(&medium)? == "Overseer"))?;
    assert_eq!(role(&replicant)?, "Replicant");
    wait_until(Duration::from_secs(10), "the dead hex to be marked lost", || {
        let s = status(&medium)?;
        Ok(s["network"]["lattice"]["hexes"].as_array().unwrap().iter().any(|h| h["status"] == "lost"))
    })?;

    // The big hex comes back but doesn't take leadership back.
    big.launch()?;
    wait_until(Duration::from_secs(20), "the big hex to rejoin", || Ok(active_hexes(&medium)? == 3))?;
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(role(&medium)?, "Overseer", "the sitting Overseer keeps the role");
    assert_eq!(role(&big)?, "Harvester");
    Ok(())
}

#[test]
fn isolated_hex_is_overseer_of_its_own_lattice() -> Result<()> {
    let server = TestServer::start()?;
    assert_eq!(role(&server)?, "Overseer");
    let s = status(&server)?;
    let me = &s["network"]["lattice"]["hexes"][0];
    assert_eq!(me["is_self"], true);
    assert!(me["api_endpoint"].as_str().unwrap().starts_with("127.0.0.1:"));
    Ok(())
}

fn doc_on(server: &TestServer, tess: &str, id: &str) -> Result<Option<Value>> {
    server.get_doc(tess, id)
}

fn count_on(server: &TestServer, tess: &str) -> Result<usize> {
    let res = server.request(Method::GET, &format!("/{}/count", tess), None, &[])?;
    Ok(if res.status.is_success() { res.body["count"].as_u64().unwrap_or(0) as usize } else { 0 })
}

fn replication(server: &TestServer) -> Result<Value> {
    Ok(status(server)?["replication"].clone())
}

#[test]
fn replicas_follow_the_overseer_and_resync_after_failover() -> Result<()> {
    let cluster = Cluster::new(3)?;
    let mut big = TestServer::start_with(cluster.options(0, 4096, "auto"))?;
    assert_eq!(role(&big)?, "Overseer");

    // Data written before the replicas exist arrives by full sync.
    let notes: Vec<Value> = (0..120).map(|i| json!({ "n": i, "status": if i % 2 == 0 { "even" } else { "odd" } })).collect();
    assert!(big.request(Method::POST, "/notes/_bulk", Some(&Value::Array(notes)), &[])?.status.is_success());
    let res = big.request(Method::POST, "/tessellations/notes/indexes", Some(&json!({ "fields": ["status"] })), &[])?;
    assert_eq!(res.status.as_u16(), 201, "{}", res.body);
    let res = big.request(Method::POST, "/users", Some(&json!({ "login": "ada", "password": "correct horse battery", "email_address": "ada@example.com", "roles": [] })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    big.insert("doomed", &json!({ "x": 1 }))?;

    let medium = TestServer::start_with(cluster.options(1, 2048, "auto"))?;
    let replicant = TestServer::start_with(cluster.options(2, 8192, "replicant"))?;
    for replica in [&medium, &replicant] {
        // The full sync copies tessellations one at a time; wait for it to finish.
        wait_until(Duration::from_secs(30), "the full sync", || {
            Ok(replication(replica)?["state"] == "streaming" && count_on(replica, "notes")? == 120)
        })?;
        let users = replica.request(Method::GET, "/users/ada", None, &[])?;
        assert_eq!(users.status.as_u16(), 200, "users replicate: {}", users.body);
        let indexes = replica.request(Method::GET, "/tessellations/notes/indexes", None, &[])?;
        assert_eq!(indexes.body["indexes"][0]["name"], "status", "{}", indexes.body);
    }
    assert_eq!(role(&medium)?, "Harvester");
    assert_eq!(role(&replicant)?, "Replicant");

    // Every kind of write streams to the replicas.
    let id = big.insert("notes", &json!({ "text": "live" }))?;
    big.patch("notes", &json!({ "id": id, "text": "patched" }))?;
    let victim = big.insert("notes", &json!({ "text": "to delete" }))?;
    big.delete("notes", &victim)?;
    let res = big.request(
        Method::POST,
        "/transactions",
        Some(&json!({ "operations": [
            { "op": "insert", "tessellation": "ledger", "data": { "amount": 5 } },
            { "op": "patch", "tessellation": "notes", "id": id, "data": { "tx": true } },
        ]})),
        &[],
    )?;
    assert_eq!(res.status.as_u16(), 200, "{}", res.body);
    big.delete_tessellation("doomed")?;
    for replica in [&medium, &replicant] {
        wait_until(Duration::from_secs(20), "streamed writes", || {
            Ok(doc_on(replica, "notes", &id)? == Some(json!({ "id": id, "text": "patched", "tx": true })))
        })?;
        assert!(doc_on(replica, "notes", &victim)?.is_none());
        assert_eq!(count_on(replica, "ledger")?, 1);
        wait_until(Duration::from_secs(10), "the tessellation drop", || {
            Ok(replica.request(Method::GET, "/tessellations/doomed", None, &[])?.status.as_u16() == 404)
        })?;
        // Indexed queries on a replica see replicated data.
        let q = replica.request(Method::POST, "/notes/_query", Some(&json!({ "filter": { "status": "even" } })), &[])?;
        assert_eq!(q.body["total"], 60);
        assert_eq!(q.body["plan"]["indexes"], json!(["status"]));
    }
    wait_until(Duration::from_secs(10), "zero lag", || {
        let r = replication(&medium)?;
        Ok(r["state"] == "streaming" && r["lag"] == 0)
    })?;

    // Replicas refuse writes and point at the Overseer.
    let res = medium.request(Method::POST, "/notes", Some(&json!({ "x": 1 })), &[])?;
    assert_eq!(res.status.as_u16(), 421, "{}", res.body);
    assert_eq!(res.error_code(), Some("read_only_replica"));
    assert!(res.body["error"]["message"].as_str().unwrap().contains(&big.url("").trim_start_matches("http://").trim_end_matches('/').to_string()));
    let res = medium.request(Method::POST, "/tessellations", Some(&json!({ "name": "nope" })), &[])?;
    assert_eq!(res.status.as_u16(), 421);
    let gql = medium.request(Method::POST, "/graphql", Some(&json!({ "query": "mutation { insertDocument(tessellation: \"notes\", data: {}) { id } }" })), &[])?;
    assert_eq!(gql.body["errors"][0]["extensions"]["code"], "READ_ONLY_REPLICA", "{}", gql.body);

    // Fail over: the medium hex leads, accepts writes, and the replicant follows it.
    big.kill();
    wait_until(Duration::from_secs(20), "failover", || Ok(role(&medium)? == "Overseer"))?;
    let after_failover = medium.insert("notes", &json!({ "text": "written to the new Overseer" }))?;
    wait_until(Duration::from_secs(30), "the replicant to follow the new Overseer", || {
        Ok(doc_on(&replicant, "notes", &after_failover)?.is_some())
    })?;

    // The old Overseer comes back as a replica and catches up.
    big.launch()?;
    wait_until(Duration::from_secs(30), "the old Overseer to resync", || Ok(doc_on(&big, "notes", &after_failover)?.is_some()))?;
    assert_eq!(role(&big)?, "Harvester");
    assert_eq!(count_on(&big, "notes")?, count_on(&medium, "notes")?);
    let res = big.request(Method::POST, "/notes", Some(&json!({ "x": 1 })), &[])?;
    assert_eq!(res.status.as_u16(), 421);
    Ok(())
}
