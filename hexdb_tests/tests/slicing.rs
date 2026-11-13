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

/// A hex that once led, now a replica of the hex that took over, gives up
/// slices; restarted, it must still say it keeps only the slices it has, or
/// the others give up their copies and, once it leads again, it never collects
/// what it deleted. (The playground lost documents in 14 slices this way.)
#[test]
fn former_overseer_restarted_as_replica_keeps_its_own_slice_count() -> Result<()> {
    let ports = [TestServer::free_port()?, TestServer::free_port()?, TestServer::free_port()?];
    let mut a = TestServer::start_with(hex(&ports, 0, 4096))?;
    let mut others = [TestServer::start_with(hex(&ports, 1, 2048))?, TestServer::start_with(hex(&ports, 2, 1024))?];
    seed(&a, 0, 300)?;
    wait_until(Duration::from_secs(90), "the replicas to split the slices", || {
        let (x, y) = (kept(&others[0])?, kept(&others[1])?);
        Ok(x.0 + y.0 == 64 && x.0 > 0 && y.0 > 0 && x.1 + y.1 == 300)
    })?;

    // A steps down; a replica (whichever the election picks) leads and collects every slice.
    a.stop()?;
    let mut leader = None;
    wait_until(Duration::from_secs(90), "a replica to lead with every document", || {
        for (i, server) in others.iter().enumerate() {
            if status(server)?["hex_type"] == "Overseer" && server.request(Method::GET, "/items/count", None, &[])?.body["count"] == 300 {
                leader = Some(i);
                return Ok(true);
            }
        }
        Ok(false)
    })?;
    let leader = leader.unwrap();
    let follower = 1 - leader;

    // A comes back as a replica, and A and the other replica split the slices again.
    a.launch()?;
    wait_until(Duration::from_secs(120), "A and the other replica to split the slices", || {
        let (x, y) = (kept(&a)?, kept(&others[follower])?);
        Ok(status(&a)?["hex_type"] != "Overseer" && x.0 < 64 && x.0 + y.0 == 64 && x.1 + y.1 == 300)
    })?;
    let before = kept(&a)?;

    // Restarted, A reports the slices it has, not every slice.
    a.restart()?;
    wait_until(Duration::from_secs(60), "A to answer after its restart", || Ok(status(&a)?["hex_type"] != "Overseer"))?;
    let after = kept(&a)?;
    assert!(after.0 < 64, "A claims every slice after a restart while it keeps {} documents of 300 (before: {:?})", after.1, before);

    // The leader goes: whichever hex takes over collects what it lacks and has every document.
    others[leader].stop()?;
    let rest = [&a, &others[follower]];
    wait_until(Duration::from_secs(120), "the new Overseer to have every document", || {
        for server in rest {
            if status(server)?["hex_type"] == "Overseer" {
                return Ok(server.request(Method::GET, "/items/count", None, &[])?.body["count"] == 300);
            }
        }
        Ok(false)
    })?;
    Ok(())
}

/// A full sync cut short (a restart, a crash) leaves a replica with some
/// tessellations copied and others not, and its old cursor claiming what it
/// held before. Such a hex must never lead: it would serve part of the data as
/// everything, and the others would copy it and drop the rest. (The playground
/// lost documents in 14 slices this way.) Alone, it waits; when a hex with the
/// data is back, that one leads and this one copies everything again.
#[test]
fn replica_whose_full_sync_was_cut_short_never_leads() -> Result<()> {
    let ports = [TestServer::free_port()?, TestServer::free_port()?];
    let mut a = TestServer::start_with(hex(&ports, 0, 1024))?;
    for tess in ["aa", "bb", "cc"] {
        let docs: Vec<Value> = (0..200).map(|i| json!({ "n": i })).collect();
        let res = a.request(Method::POST, &format!("/{}/_bulk", tess), Some(&json!(docs)), &[])?;
        assert!(res.status.is_success(), "{}", res.body);
    }

    // B (more RAM: it would win an election) stops after copying one tessellation.
    let mut options = hex(&ports, 1, 4096);
    options.env = vec![("HEXDB_DEBUG_STOP_FULL_SYNC_AFTER".into(), "1".into())];
    let mut b = TestServer::create(options)?;
    // It may stop before it's ready (the sync starts as it does) or just after.
    let _ = b.launch();
    wait_until(Duration::from_secs(60), "B to stop in the middle of its full sync", || Ok(b.has_exited()))?;

    // The Overseer goes; B comes back alone. It must not take the role.
    a.stop()?;
    b.set_options(hex(&ports, 1, 4096));
    // It can't sign anyone in (its copy of the users may be missing), so the harness's sign-in may fail.
    let _ = b.launch();
    std::thread::sleep(Duration::from_secs(8));

    // A is back: A leads with everything it had, and B copies it all again.
    a.launch()?;
    let mut last = String::new();
    let waited = wait_until(Duration::from_secs(120), "A to lead with every document, and B to have a full copy", || {
        let s = status(&a)?;
        last = format!(
            "A: {} {}; counts {:?}; lattice {}",
            s["hex_type"],
            s["replication"],
            ["aa", "bb", "cc"].map(|t| a.request(Method::GET, &format!("/{}/count", t), None, &[]).map(|r| r.body["count"].clone()).unwrap_or_default()),
            s["network"]["lattice"]["hexes"]
        );
        if s["hex_type"] != "Overseer" {
            return Ok(false);
        }
        for tess in ["aa", "bb", "cc"] {
            if a.request(Method::GET, &format!("/{}/count", tess), None, &[])?.body["count"] != 200 {
                return Ok(false);
            }
        }
        let hexes = s["network"]["lattice"]["hexes"].as_array().cloned().unwrap_or_default();
        let b_view = hexes.iter().find(|h| h["is_self"] != true);
        Ok(b_view.is_some_and(|h| h["status"] == "active" && h["role"] != "Overseer" && h["replication_state"] == "streaming"))
    });
    if let Err(e) = waited {
        bail!("{}: {}
B's log:
{}", e, last, std::fs::read_to_string(b.log_path()).unwrap_or_default().lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("
"));
    }
    for tess in ["aa", "bb", "cc"] {
        assert_eq!(a.request(Method::GET, &format!("/{}/count", tess), None, &[])?.body["count"], 200, "{}", tess);
    }
    Ok(())
}

#[test]
fn sliced_replicas_split_rebalance_and_rebuild_after_failover() -> Result<()> {
    let ports = [TestServer::free_port()?, TestServer::free_port()?, TestServer::free_port()?];
    let mut overseer = TestServer::start_with(hex(&ports, 0, 4096))?;
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

    // A failover: both replicas have applied everything before the Overseer goes.
    let published = status(&overseer)?["replication"]["applied_seq"].as_u64().unwrap_or(0);
    wait_until(Duration::from_secs(30), "the replicas to catch up", || {
        Ok(status(&first)?["replication"]["applied_seq"].as_u64() >= Some(published) && status(&second)?["replication"]["applied_seq"].as_u64() >= Some(published))
    })?;
    overseer.kill();

    // A replica is elected, collects the slices it lacks from the other, and
    // then answers with everything.
    let mut leader: Option<&TestServer> = None;
    wait_until(Duration::from_secs(90), "a new Overseer with every document", || {
        for server in [&first, &second] {
            if status(server)?["hex_type"] == "Overseer" {
                let res = server.request(Method::GET, "/items/count", None, &[])?;
                if res.status == 200 && res.body["count"] == 250 {
                    leader = Some(server);
                    return Ok(true);
                }
            }
        }
        Ok(false)
    })?;
    let leader = leader.unwrap();
    let follower = if std::ptr::eq(leader, &first) { &second } else { &first };
    assert_eq!(kept(leader)?, (64, 250));
    for n in [0, 124, 249] {
        let res = leader.request(Method::POST, "/items/_query", Some(&json!({ "filter": { "n": n } })), &[])?;
        assert_eq!(res.body["documents"].as_array().map(|d| d.len()), Some(1), "n = {}: {}", n, res.body);
    }

    // Writes work again, and the last replica (alone now) keeps everything.
    seed(leader, 250, 10)?;
    wait_until(Duration::from_secs(90), "the remaining replica to keep everything", || Ok(kept(follower)? == (64, 260)))?;
    Ok(())
}
