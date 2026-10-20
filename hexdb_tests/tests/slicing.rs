//! Sliced replicas (`replication.slicing`): placement, rebalancing when a
//! hex joins, reads through a replica, and a failover rebuilt from the slices
//! the replicas kept.

use anyhow::{bail, Result};
use hexdb_tests::{TestOptions, TestServer};
use reqwest::Method;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn wait_until(timeout: Duration, what: &str, mut check: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if check().unwrap_or(false) {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {}", what);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Hex `i` of a sliced lattice on `ports`, seeded with every other hex.
fn hex(ports: &[u16], i: usize, ram_mb: u64) -> TestOptions {
    TestOptions {
        lattice: Some(format!("slicing-{}", ports[0])),
        discovery_port: Some(ports[i]),
        peers: ports.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, p)| format!("127.0.0.1:{}", p)).collect(),
        ram_mb: Some(ram_mb),
        discovery_interval_seconds: Some(1),
        extra_toml: "\n[replication]\nslicing = true\n".into(),
        ..Default::default()
    }
}

fn status(server: &TestServer) -> Result<Value> {
    Ok(server.request(Method::GET, "/status", None, &[])?.body)
}

/// (slices kept, user documents kept) on one hex.
fn kept(server: &TestServer) -> Result<(u64, u64)> {
    let s = status(server)?;
    let r = &s["replication"];
    Ok((r["slices_held_count"].as_u64().unwrap_or(0), r["sliced_documents"].as_u64().unwrap_or(0)))
}

fn seed(server: &TestServer, from: usize, count: usize) -> Result<()> {
    let docs: Vec<Value> = (from..from + count).map(|i| json!({ "n": i })).collect();
    let res = server.request(Method::POST, "/items/_bulk", Some(&json!(docs)), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    Ok(())
}

#[test]
fn replicas_split_the_slices_and_rebalance() -> Result<()> {
    let ports = [TestServer::free_port()?, TestServer::free_port()?, TestServer::free_port()?];
    let overseer = TestServer::start_with(hex(&ports, 0, 4096))?;
    let first = TestServer::start_with(hex(&ports, 1, 1024))?;
    seed(&overseer, 0, 200)?;

    // One replica: it keeps everything.
    wait_until(Duration::from_secs(60), "the replica to copy everything", || Ok(kept(&first)? == (64, 200)))?;

    // A second replica joins: the two split the slices, and every document
    // is on exactly one of them (slice_copies = 1), besides the Overseer.
    let second = TestServer::start_with(hex(&ports, 2, 1024))?;
    wait_until(Duration::from_secs(90), "the replicas to split the slices", || {
        let (a, b) = (kept(&first)?, kept(&second)?);
        Ok(a.0 + b.0 == 64 && a.0 > 0 && b.0 > 0 && a.1 + b.1 == 200)
    })?;
    let (a, b) = (kept(&first)?, kept(&second)?);
    assert!(a.1 < 200 && b.1 < 200, "each replica keeps part: {:?} {:?}", a, b);
    assert_eq!(kept(&overseer)?.1, 200, "the Overseer keeps everything");

    // New writes land on the replica that keeps their slice.
    seed(&overseer, 200, 50)?;
    wait_until(Duration::from_secs(30), "new documents on the replicas", || Ok(kept(&first)?.1 + kept(&second)?.1 == 250))?;

    // Reads through a replica go to the Overseer, so they see everything.
    let res = second.request(Method::GET, "/items/count", None, &[])?;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(res.body["count"], 250, "{}", res.body);
    assert!(res.headers.get("x-hexdb-forwarded-to").is_some(), "answered by the Overseer");
    let sql = second.request(Method::POST, "/sql", Some(&json!({ "sql": "SELECT COUNT(*) FROM items WHERE n >= 100" })), &[])?;
    assert_eq!(sql.body["rows"], json!([[150]]), "{}", sql.body);
    // Live feeds aren't proxied: a sliced replica names the Overseer instead.
    let feed = second.request(Method::GET, "/changes/stream", None, &[])?;
    assert_eq!(feed.status, 421, "{}", feed.body);
    Ok(())
}

#[test]
fn a_failover_rebuilds_from_the_replicas() -> Result<()> {
    let ports = [TestServer::free_port()?, TestServer::free_port()?, TestServer::free_port()?];
    let mut overseer = TestServer::start_with(hex(&ports, 0, 4096))?;
    let a = TestServer::start_with(hex(&ports, 1, 1024))?;
    let b = TestServer::start_with(hex(&ports, 2, 1024))?;
    seed(&overseer, 0, 150)?;
    wait_until(Duration::from_secs(90), "the replicas to split the slices", || {
        let (x, y) = (kept(&a)?, kept(&b)?);
        Ok(x.0 + y.0 == 64 && x.0 > 0 && y.0 > 0 && x.1 + y.1 == 150)
    })?;
    // Both replicas have applied everything before the Overseer goes.
    let published = status(&overseer)?["replication"]["applied_seq"].as_u64().unwrap_or(0);
    wait_until(Duration::from_secs(30), "the replicas to catch up", || {
        Ok(status(&a)?["replication"]["applied_seq"].as_u64() >= Some(published) && status(&b)?["replication"]["applied_seq"].as_u64() >= Some(published))
    })?;

    overseer.kill();

    // A replica is elected, collects the slices it lacks from the other, and
    // then answers with everything.
    let mut leader: Option<&TestServer> = None;
    wait_until(Duration::from_secs(90), "a new Overseer with every document", || {
        for server in [&a, &b] {
            if status(server)?["hex_type"] == "Overseer" {
                let res = server.request(Method::GET, "/items/count", None, &[])?;
                if res.status == 200 && res.body["count"] == 150 {
                    leader = Some(server);
                    return Ok(true);
                }
            }
        }
        Ok(false)
    })?;
    let leader = leader.unwrap();
    let follower = if std::ptr::eq(leader, &a) { &b } else { &a };
    assert_eq!(kept(leader)?, (64, 150));
    for n in [0, 74, 149] {
        let res = leader.request(Method::POST, "/items/_query", Some(&json!({ "filter": { "n": n } })), &[])?;
        assert_eq!(res.body["documents"].as_array().map(|d| d.len()), Some(1), "n = {}: {}", n, res.body);
    }

    // Writes work again, and the last replica (alone now) keeps everything.
    seed(leader, 150, 10)?;
    wait_until(Duration::from_secs(90), "the remaining replica to keep everything", || Ok(kept(follower)? == (64, 160)))?;
    Ok(())
}
