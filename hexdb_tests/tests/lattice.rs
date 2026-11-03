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
    // Replicas with full copies, answering reads themselves (tests/slicing.rs covers sliced ones).
    let full = |options: TestOptions| TestOptions { extra_toml: format!("{}\n[replication]\nslicing = false\n", options.extra_toml), ..options };
    let mut big = TestServer::start_with(full(cluster.options(0, 4096, "auto")))?;
    assert_eq!(role(&big)?, "Overseer");

    // Data written before the replicas exist arrives by full sync.
    let notes: Vec<Value> = (0..120).map(|i| json!({ "n": i, "status": if i % 2 == 0 { "even" } else { "odd" } })).collect();
    assert!(big.request(Method::POST, "/notes/_bulk", Some(&Value::Array(notes)), &[])?.status.is_success());
    let res = big.request(Method::POST, "/tessellations/notes/indexes", Some(&json!({ "fields": ["status"] })), &[])?;
    assert_eq!(res.status.as_u16(), 201, "{}", res.body);
    let res = big.request(Method::POST, "/users", Some(&json!({ "login": "ada", "password": "correct horse battery", "email_address": "ada@example.com", "roles": [] })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    big.insert("doomed", &json!({ "x": 1 }))?;

    let medium = TestServer::start_with(full(cluster.options(1, 2048, "auto")))?;
    // The replicant refuses writes instead of forwarding them.
    let replicant = TestServer::start_with(TestOptions {
        extra_toml: "\n[replication]\nforward_writes = false\nslicing = false\n".into(),
        ..cluster.options(2, 8192, "replicant")
    })?;
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
        // Generous for slow CI runners; on a timeout, say where replication was.
        let streamed = wait_until(Duration::from_secs(60), "streamed writes", || {
            Ok(doc_on(replica, "notes", &id)? == Some(json!({ "id": id, "text": "patched", "tx": true })))
        });
        if let Err(e) = streamed {
            bail!(
                "{}: replica replication {}, document {:?}\n---- replica log ----\n{}\n---- overseer log ----\n{}",
                e,
                replication(replica)?,
                doc_on(replica, "notes", &id)?,
                replica.log_tail(40),
                big.log_tail(25)
            );
        }
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

    // A replica forwards writes to the Overseer (REST and GraphQL mutations).
    let res = medium.request(Method::POST, "/notes", Some(&json!({ "via": "replica" })), &[])?;
    assert_eq!(res.status.as_u16(), 201, "{}", res.body);
    assert!(res.headers.get("x-hexdb-forwarded-to").is_some());
    let forwarded_id = res.body["id"].as_str().unwrap().to_string();
    assert!(doc_on(&big, "notes", &forwarded_id)?.is_some(), "written on the Overseer");
    wait_until(Duration::from_secs(10), "the forwarded write to replicate back", || Ok(doc_on(&medium, "notes", &forwarded_id)?.is_some()))?;
    let gql = medium.request(Method::POST, "/graphql", Some(&json!({ "query": "mutation { insertDocument(tessellation: \"notes\", data: {}) { id } }" })), &[])?;
    assert!(gql.body["data"]["insertDocument"]["id"].is_string(), "{}", gql.body);
    let read = medium.request(Method::POST, "/graphql", Some(&json!({ "query": "{ count(tessellation: \"notes\") }" })), &[])?;
    assert!(read.headers.get("x-hexdb-forwarded-to").is_none(), "queries stay local");

    // With forwarding off, a replica refuses writes and points at the Overseer.
    let res = replicant.request(Method::POST, "/notes", Some(&json!({ "x": 1 })), &[])?;
    assert_eq!(res.status.as_u16(), 421, "{}", res.body);
    assert_eq!(res.error_code(), Some("read_only_replica"));
    assert!(res.body["error"]["message"].as_str().unwrap().contains(&big.url("").trim_start_matches("http://").trim_end_matches('/').to_string()));
    let res = replicant.request(Method::POST, "/tessellations", Some(&json!({ "name": "nope" })), &[])?;
    assert_eq!(res.status.as_u16(), 421);
    let gql = replicant.request(Method::POST, "/graphql", Some(&json!({ "query": "mutation { insertDocument(tessellation: \"notes\", data: {}) { id } }" })), &[])?;
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
    // Its writes now go to the new Overseer.
    let res = big.request(Method::POST, "/notes", Some(&json!({ "x": 1 })), &[])?;
    assert_eq!(res.status.as_u16(), 201, "{}", res.body);
    assert!(doc_on(&medium, "notes", res.body["id"].as_str().unwrap())?.is_some());
    Ok(())
}

/// A hex that can't open its discovery port can't take part in its lattice,
/// so it stops at startup and says why, instead of running unreachable.
#[test]
fn a_taken_discovery_port_stops_startup() -> Result<()> {
    let port = TestServer::free_port()?;
    let _taken = std::net::TcpListener::bind(("127.0.0.1", port))?;
    let started = TestServer::start_with(TestOptions { discovery_port: Some(port), ..Default::default() });
    let error = format!("{:#}", started.err().expect("the server must not start"));
    assert!(error.contains("Failed to bind the discovery endpoint"), "{}", error);
    Ok(())
}

#[test]
fn restarting_the_overseer_fails_no_write_and_copies_nothing_again() -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let cluster = Cluster::new(3)?;
    let mut overseer = TestServer::start_with(cluster.options(0, 4096, "auto"))?;
    let replica = TestServer::start_with(cluster.options(1, 2048, "auto"))?;
    let other = TestServer::start_with(cluster.options(2, 1024, "auto"))?;
    wait_until(Duration::from_secs(30), "all hexes to see each other", || {
        Ok([&overseer, &replica, &other].iter().map(|s| active_hexes(s)).collect::<Result<Vec<_>>>()?.iter().all(|n| *n == 3))
    })?;
    assert_eq!(role(&overseer)?, "Overseer");
    let first = overseer.insert("orders", &json!({ "n": 0 }))?;
    wait_until(Duration::from_secs(30), "the replicas' first sync", || Ok(doc_on(&replica, "orders", &first)?.is_some() && doc_on(&other, "orders", &first)?.is_some()))?;
    let synced = [replication(&replica)?["last_sync"].clone(), replication(&other)?["last_sync"].clone()];

    // Writes through a replica (forwarded to the Overseer) the whole time.
    let (url, token) = (replica.url("/orders"), replica.token().to_string());
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = stop.clone();
        std::thread::spawn(move || -> Result<(Vec<String>, Vec<String>)> {
            let http = hexdb_tests::http().timeout(Duration::from_secs(60)).build()?;
            let (mut written, mut failed) = (Vec::new(), Vec::new());
            let mut n = 0;
            while !stop.load(Ordering::Relaxed) {
                n += 1;
                match http.post(&url).bearer_auth(&token).json(&json!({ "n": n })).send() {
                    Ok(r) if r.status().is_success() => written.push(r.json::<Value>()?["id"].as_str().unwrap_or_default().to_string()),
                    Ok(r) => failed.push(format!("#{} {}: {}", n, r.status(), r.text().unwrap_or_default())),
                    Err(e) => failed.push(format!("#{} {}", n, e)),
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok((written, failed))
        })
    };
    std::thread::sleep(Duration::from_secs(2));

    // A restart (an upgrade): the replicas hold writes until the Overseer is back.
    overseer.restart()?;
    wait_until(Duration::from_secs(30), "the Overseer to be back", || Ok(role(&overseer)? == "Overseer"))?;
    std::thread::sleep(Duration::from_secs(2));
    stop.store(true, Ordering::Relaxed);
    let (written, failed) = writer.join().expect("the writer thread")?;

    assert!(failed.is_empty(), "no write may fail across the restart ({} of {} failed): {:?}", failed.len(), written.len() + failed.len(), failed);
    assert!(written.len() > 20, "writes kept flowing: {}", written.len());
    assert_eq!(role(&replica)?, "Harvester", "the restarted Overseer keeps its role");
    for id in &written {
        assert!(doc_on(&overseer, "orders", id)?.is_some(), "write {} is missing after the restart", id);
    }
    // The replicas resumed where they were instead of copying everything again.
    wait_until(Duration::from_secs(30), "the replicas to catch up", || {
        let last = written.last().unwrap();
        Ok(doc_on(&replica, "orders", last)?.is_some() && doc_on(&other, "orders", last)?.is_some())
    })?;
    assert_eq!([replication(&replica)?["last_sync"].clone(), replication(&other)?["last_sync"].clone()], synced, "no full sync after the restart");
    Ok(())
}

#[test]
fn a_stopping_overseer_hands_over_and_nothing_is_copied_again() -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // Full copies on the replicas: a stopping Overseer hands its role over (a
    // sliced lattice keeps it across the restart instead; see the test above).
    let cluster = Cluster::new(3)?;
    let full = |i: usize, ram_mb: u64| TestOptions {
        extra_toml: "\n[replication]\nslicing = false\n".into(),
        discovery_interval_seconds: Some(5),
        ..cluster.options(i, ram_mb, "auto")
    };
    let mut overseer = TestServer::start_with(full(0, 4096))?;
    let replica = TestServer::start_with(full(1, 2048))?;
    let other = TestServer::start_with(full(2, 1024))?;
    wait_until(Duration::from_secs(60), "all hexes to see each other", || {
        Ok([&overseer, &replica, &other].iter().map(|s| active_hexes(s)).collect::<Result<Vec<_>>>()?.iter().all(|n| *n == 3))
    })?;
    assert_eq!(role(&overseer)?, "Overseer");
    let first = overseer.insert("orders", &json!({ "n": 0 }))?;
    wait_until(Duration::from_secs(30), "the replicas' first sync", || Ok(doc_on(&replica, "orders", &first)?.is_some() && doc_on(&other, "orders", &first)?.is_some()))?;
    let synced = [replication(&replica)?["last_sync"].clone(), replication(&other)?["last_sync"].clone()];

    let (url, token) = (replica.url("/orders"), replica.token().to_string());
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = stop.clone();
        std::thread::spawn(move || -> Result<(Vec<String>, Vec<String>)> {
            let http = hexdb_tests::http().timeout(Duration::from_secs(60)).build()?;
            let (mut written, mut failed) = (Vec::new(), Vec::new());
            let mut n = 0;
            while !stop.load(Ordering::Relaxed) {
                n += 1;
                match http.post(&url).bearer_auth(&token).json(&json!({ "n": n })).send() {
                    Ok(r) if r.status().is_success() => written.push(r.json::<Value>()?["id"].as_str().unwrap_or_default().to_string()),
                    Ok(r) => failed.push(format!("#{} {}: {}", n, r.status(), r.text().unwrap_or_default())),
                    Err(e) => failed.push(format!("#{} {}", n, e)),
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok((written, failed))
        })
    };
    std::thread::sleep(Duration::from_secs(2));

    // A graceful stop (SIGTERM, `hexdb stop`): the role moves before the hex goes.
    let started = Instant::now();
    overseer.stop()?;
    let stopped_in = started.elapsed();
    let leader = if role(&replica)? == "Overseer" { &replica } else { &other };
    assert_eq!(role(leader)?, "Overseer", "another hex took over before the old Overseer stopped");
    std::thread::sleep(Duration::from_secs(2));
    stop.store(true, Ordering::Relaxed);
    let (written, failed) = writer.join().expect("the writer thread")?;
    assert!(failed.is_empty(), "no write may fail across the handover ({} of {} failed): {:?}", failed.len(), written.len() + failed.len(), failed);
    assert!(written.len() > 20, "writes kept flowing: {}", written.len());
    assert!(stopped_in < Duration::from_secs(15), "the handover took {:?}", stopped_in);
    for id in &written {
        assert!(doc_on(leader, "orders", id)?.is_some(), "write {} is missing after the handover", id);
    }
    // The remaining replica continued the history instead of copying it again.
    let follower = if std::ptr::eq(leader, &replica) { &other } else { &replica };
    let last = written.last().unwrap();
    wait_until(Duration::from_secs(30), "the replica to follow the new Overseer", || Ok(doc_on(follower, "orders", last)?.is_some()))?;
    let index = if std::ptr::eq(follower, &replica) { 0 } else { 1 };
    assert_eq!(replication(follower)?["last_sync"], synced[index], "no full sync after the handover");

    // The old Overseer comes back as a replica, also without a full sync.
    overseer.launch()?;
    wait_until(Duration::from_secs(60), "the old Overseer to follow", || Ok(replication(&overseer)?["state"] == "streaming"))?;
    let after = leader.insert("orders", &json!({ "n": "after" }))?;
    wait_until(Duration::from_secs(30), "it to catch up", || Ok(doc_on(&overseer, "orders", &after)?.is_some()))?;
    assert!(replication(&overseer)?["last_sync"].is_null(), "the old Overseer resumed: {}", replication(&overseer)?);
    assert_eq!(role(leader)?, "Overseer");
    Ok(())
}

#[test]
fn a_hex_learns_the_hexes_that_probe_it() -> Result<()> {
    // The first hex has no seeds at all; the second and third know only the
    // first. Each probe tells the first where the prober is, so it probes
    // them back, and through it they find each other.
    let cluster = Cluster::new(3)?;
    let seedless = TestServer::start_with(TestOptions { peers: Vec::new(), ..cluster.options(0, 4096, "auto") })?;
    let first = format!("127.0.0.1:{}", cluster.ports[0]);
    let joiner = TestServer::start_with(TestOptions { peers: vec![first.clone()], ..cluster.options(1, 2048, "auto") })?;
    let other = TestServer::start_with(TestOptions { peers: vec![first], ..cluster.options(2, 1024, "auto") })?;
    let seen = || [&seedless, &joiner, &other].iter().map(|s| active_hexes(s)).collect::<Result<Vec<_>>>();
    let waited = wait_until(Duration::from_secs(60), "every hex to see the other two", || Ok(seen()?.iter().all(|n| *n == 3)));
    assert!(waited.is_ok(), "hexes each see (seedless, joiner, other): {:?}", seen()?);
    assert_eq!(role(&seedless)?, "Overseer");
    Ok(())
}

#[test]
fn hexes_report_their_regions_and_the_lattice_counts_them() -> Result<()> {
    let cluster = Cluster::new(2)?;
    let in_region = |i: usize, ram_mb: u64, region: &str| TestOptions { identity_toml: format!("region = \"{}\"", region), ..cluster.options(i, ram_mb, "auto") };
    let iad = TestServer::start_with(in_region(0, 4096, "iad"))?;
    let lhr = TestServer::start_with(in_region(1, 2048, "lhr"))?;
    wait_until(Duration::from_secs(20), "both hexes to see each other", || Ok(active_hexes(&iad)? == 2 && active_hexes(&lhr)? == 2))?;
    for server in [&iad, &lhr] {
        let lattice = status(server)?["network"]["lattice"].clone();
        assert_eq!(lattice["regions"], json!(["iad", "lhr"]), "{}", lattice);
        let mut regions: Vec<(String, String)> = lattice["hexes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| (h["role"].as_str().unwrap_or_default().to_string(), h["region"].as_str().unwrap_or_default().to_string()))
            .collect();
        regions.sort();
        assert_eq!(regions, vec![("Harvester".to_string(), "lhr".to_string()), ("Overseer".to_string(), "iad".to_string())]);
    }
    Ok(())
}
