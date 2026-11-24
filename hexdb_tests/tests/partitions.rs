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

/// Documents in `tess` on this hex's partition only (a plain count merges every partition's).
fn local_count(server: &TestServer, tess: &str) -> Result<u64> {
    let res = server.request(Method::GET, &format!("/{}/count", tess), None, &[("x-hexdb-partition-local", "1")])?;
    Ok(res.body["count"].as_u64().unwrap_or(u64::MAX))
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

    // New documents get IDs in this hex's partition, so inserts land here.
    for n in 0..40 {
        let res = server.request(Method::POST, "/orders", Some(&json!({ "n": n })), &[])?;
        assert_eq!(res.status, 201, "{}", res.body);
        let id: ulid::Ulid = res.body["id"].as_str().unwrap_or_default().parse()?;
        assert!(hexdb_core::slicing::slice_of("orders", id) < 32, "a document for partition 1 was stored here");
    }
    assert_eq!(local_count(&server, "orders")?, 40);
    // A write to a document of partition 1 (no hex leads it yet) is refused untouched.
    let theirs = loop {
        let id = ulid::Ulid::generate();
        if hexdb_core::slicing::slice_of("orders", id) >= 32 {
            break id;
        }
    };
    let res = server.request(Method::PUT, &format!("/orders/{}", theirs), Some(&json!({ "n": 1 })), &[])?;
    assert_eq!(res.status, 503, "{}", res.body);
    assert_eq!(res.body["error"]["code"], "partition_unavailable", "{}", res.body);
    // A transaction naming it is refused as crossing partitions, and so are upserts.
    let tx = json!({ "operations": [{ "op": "insert", "tessellation": "orders", "data": { "n": 1 } }, { "op": "replace", "tessellation": "orders", "id": theirs.to_string(), "data": { "n": 2 } }] });
    let res = server.request(Method::POST, "/transactions", Some(&tx), &[])?;
    assert_eq!(res.status, 409, "{}", res.body);
    assert!(res.body.to_string().contains("cross_partition"), "{}", res.body);
    let res = server.request(Method::POST, "/orders/_upsert", Some(&json!({ "key": ["n"], "documents": [{ "n": 1 }] })), &[])?;
    assert_eq!(res.status, 409, "{}", res.body);
    assert_eq!(local_count(&server, "orders")?, 40);

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

    // Each hex's inserts land in its own partition's slices.
    let mut on_b = None;
    for (server, low) in [(&a, true), (&b, false)] {
        for n in 0..20 {
            let res = server.request(Method::POST, "/orders", Some(&json!({ "n": n })), &[])?;
            assert_eq!(res.status, 201, "{}", res.body);
            let id: ulid::Ulid = res.body["id"].as_str().unwrap_or_default().parse()?;
            assert_eq!(hexdb_core::slicing::slice_of("orders", id) < 32, low, "a document landed in the wrong partition");
            if !low {
                on_b = Some(id);
            }
        }
    }
    assert_eq!((local_count(&a, "orders")?, local_count(&b, "orders")?), (20, 20), "each partition holds exactly its own documents");
    // A change to partition 1's document sent to A is passed to B, which owns it.
    let id = on_b.unwrap();
    let res = a.request(Method::PATCH, &format!("/orders/{}", id), Some(&json!({ "n": 99 })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    assert_eq!(b.get_doc("orders", &id.to_string())?.map(|d| d["n"].clone()), Some(json!(99)));
    // Reads: one document from either hex; queries and counts from both partitions, merged.
    assert_eq!(a.get_doc("orders", &id.to_string())?.map(|d| d["n"].clone()), Some(json!(99)), "A passes the read to B");
    let local = a.request(Method::GET, "/orders/count", None, &[("x-hexdb-partition-local", "1")])?;
    assert_eq!(local.body["count"], 20, "{}", local.body);
    for server in [&a, &b] {
        assert_eq!(server.request(Method::GET, "/orders/count", None, &[])?.body["count"], 40);
        let res = server.request(Method::POST, "/orders/_query", Some(&json!({ "sort": "-n", "limit": 5, "offset": 2, "fields": ["n"] })), &[])?;
        assert_eq!(res.status, 200, "{}", res.body);
        assert_eq!(res.body["total"], 40, "{}", res.body);
        let ns: Vec<i64> = res.body["documents"].as_array().unwrap().iter().filter_map(|d| d["n"].as_i64()).collect();
        // 99 (B's 19, patched), A's 19, then 18, 18, 17, 17, ...: offset 2 skips the first two.
        assert_eq!(ns, vec![18, 18, 17, 17, 16], "{}", res.body);
        let listed = server.request(Method::GET, "/orders?sort=n&limit=3", None, &[])?;
        let ns: Vec<i64> = listed.body["documents"].as_array().unwrap().iter().filter_map(|d| d["n"].as_i64()).collect();
        assert_eq!(ns, vec![0, 0, 1], "{}", listed.body);
    }
    // The change feed merges both partitions' feeds, with a cursor per partition.
    let start = a.request(Method::GET, "/changes", None, &[])?;
    assert_eq!(start.status, 200, "{}", start.body);
    let cursor = start.body["cursor"].as_str().unwrap_or_default().to_string();
    assert!(cursor.contains("0:") && cursor.contains("1:"), "{}", start.body);
    a.request(Method::POST, "/orders", Some(&json!({ "n": 500 })), &[])?;
    b.request(Method::POST, "/orders", Some(&json!({ "n": 501 })), &[])?;
    let feed = b.request(Method::GET, &format!("/changes?after={}&tessellation=orders&wait=5", cursor), None, &[])?;
    assert_eq!(feed.status, 200, "{}", feed.body);
    let seen: Vec<(i64, i64)> = feed.body["changes"].as_array().unwrap().iter().filter_map(|c| Some((c["document"]["n"].as_i64()?, c["partition"].as_i64()?))).collect();
    assert!(seen.contains(&(500, 0)) && seen.contains(&(501, 1)), "{}", feed.body);
    let again = a.request(Method::GET, &format!("/changes?after={}&tessellation=orders", feed.body["cursor"].as_str().unwrap_or_default()), None, &[])?;
    assert_eq!(again.body["changes"].as_array().map(Vec::len), Some(0), "nothing twice: {}", again.body);
    let res = a.request(Method::GET, "/changes/stream", None, &[])?;
    assert_eq!(res.status, 409, "{}", res.body);
    // Back to the documents: the two just added are counted everywhere.
    assert_eq!(a.request(Method::GET, "/orders/count", None, &[])?.body["count"], 42);

    // No hex holds a partitioned lattice whole: a lattice backup is refused, partition by partition instead.
    let res = a.request(Method::POST, "/backup", None, &[])?;
    assert_eq!(res.status, 503, "{}", res.body);
    assert!(res.body.to_string().contains("partition by partition"), "{}", res.body);

    // What can't be merged yet is refused, not answered from one partition.
    for (path, body) in [("/orders/_aggregate", json!({ "group_by": [], "aggregates": [{ "op": "count" }] })), ("/sql", json!({ "query": "SELECT COUNT(*) FROM orders" }))] {
        let res = a.request(Method::POST, path, Some(&body), &[])?;
        assert_eq!(res.status, 409, "{} {}", path, res.body);
        assert_eq!(res.body["error"]["code"], "cross_partition", "{}", res.body);
    }

    // A user made on partition 0 reaches partition 1 through the system stream.
    let res = a.request(Method::POST, "/users", Some(&json!({ "login": "ada", "password": "correct horse battery", "email_address": "ada@example.com", "roles": [] })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    wait_until(Duration::from_secs(30), "the user to reach partition 1", || {
        Ok(b.request(Method::GET, "/users", None, &[])?.body.to_string().contains("\"ada\""))
    })?;
    // A user made through partition 1 is passed to partition 0, which keeps users.
    let res = b.request(Method::POST, "/users", Some(&json!({ "login": "bob", "password": "correct horse battery", "email_address": "bob@example.com", "roles": [] })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    assert!(a.request(Method::GET, "/users", None, &[])?.body.to_string().contains("\"bob\""));
    wait_until(Duration::from_secs(30), "bob to reach partition 1", || Ok(b.request(Method::GET, "/users", None, &[])?.body.to_string().contains("\"bob\"")))?;

    // An index defined on partition 0 reaches partition 1 with the catalog.
    let res = a.request(Method::POST, "/tessellations/orders/indexes", Some(&json!({ "name": "by_n", "fields": ["n"] })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    wait_until(Duration::from_secs(30), "the index to reach partition 1", || {
        Ok(b.request(Method::GET, "/tessellations/orders/indexes", None, &[])?.body.to_string().contains("by_n"))
    })?;

    // Partition 1 keeps its own documents and the system tessellations through a restart.
    let mut b = b;
    let before = local_count(&b, "orders")?;
    b.restart()?;
    wait_until(Duration::from_secs(30), "B to lead partition 1 again", || Ok(role(&b)? == "Overseer"))?;
    assert_eq!(local_count(&b, "orders")?, before);
    assert!(b.request(Method::GET, "/users", None, &[])?.body.to_string().contains("\"ada\""));
    Ok(())
}
