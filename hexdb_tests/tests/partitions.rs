//! Partitioned writes, stage 2 (a preview, `partitioning.enabled`): setting the
//! partition map, which a lattice accepts with several partitions only while it
//! holds no user documents, and a hex taking user writes only for the slices its
//! partition owns.

use anyhow::Result;
use hexdb_tests::{TestOptions, TestServer, TEST_ADMIN_PASSWORD};
use reqwest::Method;
use serde_json::{json, Value};

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
