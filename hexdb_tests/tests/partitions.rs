//! Partitioned writes, stage 2 (a preview, `partitioning.enabled`): setting the
//! partition map, which a lattice accepts with several partitions only while it
//! holds no user documents, and a hex taking user writes only for the slices its
//! partition owns.

use anyhow::{bail, Result};
use hexdb_tests::{TestOptions, TestServer, TEST_ADMIN_PASSWORD};
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
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// A hex of a two-hex lattice with partitioning on.
fn lattice_hex(ports: &[u16], i: usize, ram_mb: u64) -> TestOptions {
    TestOptions {
        lattice: Some(format!("partitions-{}", ports[0])),
        discovery_port: Some(ports[i]),
        peers: ports.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, p)| format!("127.0.0.1:{}", p)).collect(),
        ram_mb: Some(ram_mb),
        discovery_interval_seconds: Some(1),
        extra_toml: "\n[partitioning]\nenabled = true\n".into(),
        ..Default::default()
    }
}

fn enabled() -> TestOptions {
    TestOptions { extra_toml: "\n[partitioning]\nenabled = true\n".into(), ..Default::default() }
}

fn hex_id(server: &TestServer) -> Result<String> {
    Ok(server.request(Method::GET, "/status", None, &[])?.body["id"].as_str().unwrap_or_default().to_string())
}

/// Two partitions: the low 32 slices with this hex, the high 32 with no hex yet.
fn split(me: &str) -> Value {
    json!([
        { "id": 0, "slices": (0..32).collect::<Vec<u32>>(), "hexes": [me] },
        { "id": 1, "slices": (32..64).collect::<Vec<u32>>(), "hexes": [] },
    ])
}

#[test]
fn a_lattice_is_split_only_while_empty_and_then_writes_only_its_own_slices() -> Result<()> {
    let server = TestServer::start_with(enabled())?;
    let me = hex_id(&server)?;
    let put = |password: &str, partitions: Value| server.request(Method::PUT, "/status/partitions", Some(&json!({ "password": password, "partitions": partitions })), &[]);

    // The password is checked again.
    assert_eq!(put("wrong password", split(&me))?.status, 403);

    // Not while it holds documents.
    let id = server.insert("orders", &json!({ "n": 1 }))?;
    let res = put(TEST_ADMIN_PASSWORD, split(&me))?;
    assert_eq!(res.status, 409, "{}", res.body);
    assert!(res.body.to_string().contains("only while it's empty"), "{}", res.body);
    assert_eq!(server.request(Method::GET, "/status/partitions", None, &[])?.body["epoch"], 1);

    // Invalid maps are refused: a slice in no partition.
    server.delete("orders", &id)?;
    let gap = json!([{ "id": 0, "slices": (0..32).collect::<Vec<u32>>(), "hexes": [me] }]);
    assert_eq!(put(TEST_ADMIN_PASSWORD, gap)?.status, 400);

    // Empty: accepted.
    let res = put(TEST_ADMIN_PASSWORD, split(&me))?;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(res.body["epoch"], 2, "{}", res.body);
    assert_eq!(res.body["this_hex"], 0, "{}", res.body);
    assert_eq!(res.body["partitions"][1]["slices"].as_array().map(Vec::len), Some(32), "{}", res.body);

    // A user write lands only in a slice this hex's partition owns; one for partition 1
    // (no hex holds it yet) is refused before anything is written.
    let (mut stored, mut refused) = (0, 0);
    for n in 0..40 {
        let res = server.request(Method::POST, "/orders", Some(&json!({ "n": n })), &[])?;
        match res.status.as_u16() {
            201 => {
                let id: ulid::Ulid = res.body["id"].as_str().unwrap_or_default().parse()?;
                assert!(hexdb_core::slicing::slice_of("orders", id) < 32, "a document for partition 1 was stored here");
                stored += 1;
            }
            503 => {
                assert_eq!(res.body["error"]["code"], "partition_unavailable", "{}", res.body);
                refused += 1;
            }
            other => panic!("unexpected {}: {}", other, res.body),
        }
    }
    assert!(stored > 0 && refused > 0, "stored {}, refused {}", stored, refused);
    assert_eq!(server.count("orders")?, stored);

    // System writes (users) aren't placed by hash: they always work on partition 0.
    let user = server.request(Method::POST, "/users", Some(&json!({ "login": "ada", "password": "correct horse battery", "email_address": "ada@example.com", "roles": [] })), &[])?;
    assert!(user.status.is_success(), "{}", user.body);

    // Now that it holds documents, the map doesn't change again.
    let res = put(TEST_ADMIN_PASSWORD, json!([{ "id": 0, "slices": (0..64).collect::<Vec<u32>>(), "hexes": [me] }]))?;
    assert_eq!(res.status, 409, "{}", res.body);
    Ok(())
}

#[test]
fn the_map_stays_put_while_partitioning_is_off() -> Result<()> {
    let server = TestServer::start()?;
    let me = hex_id(&server)?;
    let res = server.request(Method::PUT, "/status/partitions", Some(&json!({ "password": TEST_ADMIN_PASSWORD, "partitions": split(&me) })), &[])?;
    assert_eq!(res.status, 400, "{}", res.body);
    assert!(res.body.to_string().contains("partitioning.enabled"), "{}", res.body);
    Ok(())
}

/// Two partitions of one hex each: each hex leads its own group, writes land only in
/// the partition that owns them, and partition 0's users, keys and tessellation
/// definitions reach the other partition through the system stream.
#[test]
fn each_partition_leads_itself_and_partition_zero_serves_the_system_tessellations() -> Result<()> {
    let ports = [TestServer::free_port()?, TestServer::free_port()?];
    let a = TestServer::start_with(lattice_hex(&ports, 0, 4096))?;
    let b = TestServer::start_with(lattice_hex(&ports, 1, 1024))?;
    let role = |s: &TestServer| -> Result<String> { Ok(s.request(Method::GET, "/status", None, &[])?.body["hex_type"].as_str().unwrap_or_default().to_string()) };
    wait_until(Duration::from_secs(30), "one Overseer and a replica", || Ok(role(&a)? == "Overseer" && role(&b)? != "Overseer"))?;
    let (ida, idb) = (hex_id(&a)?, hex_id(&b)?);

    let map = json!([
        { "id": 0, "slices": (0..32).collect::<Vec<u32>>(), "hexes": [ida] },
        { "id": 1, "slices": (32..64).collect::<Vec<u32>>(), "hexes": [idb] },
    ]);
    let res = a.request(Method::PUT, "/status/partitions", Some(&json!({ "password": TEST_ADMIN_PASSWORD, "partitions": map })), &[])?;
    assert_eq!(res.status, 200, "{}", res.body);

    // B learns the map and leads partition 1; A keeps leading partition 0.
    wait_until(Duration::from_secs(30), "B to lead partition 1", || {
        let p = b.request(Method::GET, "/status/partitions", None, &[])?.body;
        Ok(p["epoch"] == 2 && p["this_hex"] == 1 && role(&b)? == "Overseer")
    })?;
    assert_eq!(role(&a)?, "Overseer");

    // Each takes user writes only for its own slices.
    for (server, low) in [(&a, true), (&b, false)] {
        let mut stored = 0;
        for n in 0..30 {
            let res = server.request(Method::POST, "/orders", Some(&json!({ "n": n })), &[])?;
            if res.status == 201 {
                let id: ulid::Ulid = res.body["id"].as_str().unwrap_or_default().parse()?;
                assert_eq!(hexdb_core::slicing::slice_of("orders", id) < 32, low, "a document landed in the wrong partition");
                stored += 1;
            } else {
                assert_eq!(res.body["error"]["code"], "partition_unavailable", "{}", res.body);
            }
        }
        assert!(stored > 0);
    }

    // A user made on partition 0 reaches partition 1 through the system stream.
    let res = a.request(Method::POST, "/users", Some(&json!({ "login": "ada", "password": "correct horse battery", "email_address": "ada@example.com", "roles": [] })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    wait_until(Duration::from_secs(30), "the user to reach partition 1", || {
        Ok(b.request(Method::GET, "/users", None, &[])?.body.to_string().contains("\"ada\""))
    })?;
    // Partition 1 doesn't make users itself (they're partition 0's).
    let res = b.request(Method::POST, "/users", Some(&json!({ "login": "bob", "password": "correct horse battery", "email_address": "bob@example.com", "roles": [] })), &[])?;
    assert_eq!(res.status, 503, "{}", res.body);
    assert_eq!(res.body["error"]["code"], "partition_unavailable", "{}", res.body);

    // An index defined on partition 0 reaches partition 1 with the catalog.
    let res = a.request(Method::POST, "/tessellations/orders/indexes", Some(&json!({ "name": "by_n", "fields": ["n"] })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    wait_until(Duration::from_secs(30), "the index to reach partition 1", || {
        Ok(b.request(Method::GET, "/tessellations/orders/indexes", None, &[])?.body.to_string().contains("by_n"))
    })?;

    // Partition 1 keeps its own documents and the system tessellations through a restart.
    let mut b = b;
    let before = b.count("orders")?;
    b.restart()?;
    wait_until(Duration::from_secs(30), "B to lead partition 1 again", || Ok(role(&b)? == "Overseer"))?;
    assert_eq!(b.count("orders")?, before);
    assert!(b.request(Method::GET, "/users", None, &[])?.body.to_string().contains("\"ada\""));
    Ok(())
}
