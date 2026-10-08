//! Read paths: cursor paging across memory and disk, cached counts that
//! follow TTLs, index-ordered sorting, and field projection.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::BTreeSet;

fn ids(body: &Value) -> Vec<String> {
    body["documents"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect()
}

#[test]
fn cursor_paging_sees_exactly_the_live_documents() -> Result<()> {
    let server = TestServer::start()?;
    // Some documents end up in SSTables, some only in memory; some are deleted or replaced.
    let first: Vec<Value> = (0..400).map(|i| json!({ "n": i })).collect();
    let res = server.request(Method::POST, "/items/_bulk", Some(&Value::Array(first)), &[])?;
    let mut live: BTreeSet<String> = res.body["ids"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    server.flush()?;
    let second: Vec<Value> = (400..700).map(|i| json!({ "n": i })).collect();
    let res = server.request(Method::POST, "/items/_bulk", Some(&Value::Array(second)), &[])?;
    live.extend(res.body["ids"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()));
    let all: Vec<String> = live.iter().cloned().collect();
    for id in all.iter().step_by(3) {
        server.delete("items", id)?;
        live.remove(id);
    }
    for id in all.iter().skip(1).step_by(7) {
        if live.contains(id) {
            server.patch("items", &json!({ "id": id, "patched": true }))?;
        }
    }
    let ttl = server.insert_with_query("items", "?ttl=1", &json!({ "short": true }))?;

    std::thread::sleep(std::time::Duration::from_millis(1500));
    let _ = ttl; // expired: never listed
    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let path = match &after {
            Some(a) => format!("/items?limit=37&after={}", a),
            None => "/items?limit=37".to_string(),
        };
        let page = server.request(Method::GET, &path, None, &[])?.body;
        assert_eq!(page["total"].as_u64().unwrap() as usize, live.len(), "total covers every page");
        seen.extend(ids(&page));
        match page["next"].as_str() {
            Some(next) => after = Some(next.to_string()),
            None => break,
        }
    }
    let expected: Vec<String> = live.iter().cloned().collect();
    assert_eq!(seen, expected, "every live document once, in ID order");
    assert_eq!(server.count("items")?, live.len());
    Ok(())
}

#[test]
fn cached_counts_follow_writes_and_expiry() -> Result<()> {
    let server = TestServer::start()?;
    server.insert("items", &json!({ "a": 1 }))?;
    assert_eq!(server.count("items")?, 1);
    server.insert_with_query("items", "?ttl=2", &json!({ "a": 2 }))?;
    assert_eq!(server.count("items")?, 2, "a write invalidates the cached count");
    std::thread::sleep(std::time::Duration::from_millis(2600));
    assert_eq!(server.count("items")?, 1, "the cached count expires with the earliest TTL");
    let status = server.request(Method::GET, "/status", None, &[])?;
    let items = status.body["metrics"]["tessellations"].as_array().unwrap().iter().find(|t| t["name"] == "items").cloned().unwrap();
    assert_eq!(items["document_count"], 1);
    Ok(())
}

#[test]
fn index_ordered_sorting_matches_an_in_memory_sort() -> Result<()> {
    let server = TestServer::start()?;
    let docs: Vec<Value> = (0..150)
        .map(|i| match i % 6 {
            0 => json!({ "rank": (i * 7919) % 101 }),
            1 => json!({ "rank": format!("s{:03}", (i * 31) % 97) }),
            2 => json!({ "other": i }),
            3 => json!({ "rank": null }),
            4 => json!({ "rank": (i % 2 == 0) }),
            _ => json!({ "rank": (i as f64) / 3.0 }),
        })
        .collect();
    server.request(Method::POST, "/ranked/_bulk", Some(&Value::Array(docs)), &[])?;
    let query = |sort: &str, offset: usize| -> Result<Value> {
        Ok(server.request(Method::POST, "/ranked/_query", Some(&json!({ "sort": sort, "limit": 20, "offset": offset })), &[])?.body)
    };
    let mut before = Vec::new();
    for (sort, offset) in [("rank", 0), ("rank", 40), ("-rank", 0), ("-rank", 130)] {
        before.push(ids(&query(sort, offset)?));
    }

    let res = server.request(Method::POST, "/tessellations/ranked/indexes", Some(&json!({ "fields": ["rank"] })), &[])?;
    assert_eq!(res.status.as_u16(), 201);
    for (i, (sort, offset)) in [("rank", 0), ("rank", 40), ("-rank", 0), ("-rank", 130)].into_iter().enumerate() {
        let page = query(sort, offset)?;
        assert_eq!(ids(&page), before[i], "{} offset {}", sort, offset);
        assert_eq!(page["plan"]["indexes"], json!(["rank"]), "{}", sort);
        assert_eq!(page["total"], 150);
    }
    let page = query("rank", 0)?;
    assert!(page["plan"]["scanned"].as_u64().unwrap() <= 21, "stops at the page: {}", page["plan"]);

    // An array value sorts by its first element but is indexed under each one:
    // pages that reach it fall back to the in-memory sort, and results stay the same.
    server.insert("ranked", &json!({ "rank": [500, -500] }))?;
    let offsets = [("rank", 0), ("rank", 60), ("rank", 140), ("-rank", 0), ("-rank", 140)];
    let with_index: Vec<Vec<String>> = offsets.iter().map(|(s, o)| query(s, *o).map(|p| ids(&p))).collect::<Result<_>>()?;
    server.request(Method::DELETE, "/tessellations/ranked/indexes/rank", None, &[])?;
    for (i, (s, o)) in offsets.iter().enumerate() {
        assert_eq!(with_index[i], ids(&query(s, *o)?), "{} offset {}", s, o);
    }
    Ok(())
}

#[test]
fn field_projection_returns_only_requested_fields() -> Result<()> {
    let server = TestServer::start()?;
    let id = server.insert("people", &json!({ "name": "Ada", "address": { "city": "London", "zip": "N1" }, "secret": 1 }))?;
    let res = server.request(Method::GET, &format!("/people?fields=name,address.city"), None, &[])?;
    assert_eq!(res.body["documents"][0], json!({ "id": id, "name": "Ada", "address": { "city": "London" } }));
    let res = server.request(Method::POST, "/people/_query", Some(&json!({ "fields": ["secret"] })), &[])?;
    assert_eq!(res.body["documents"][0], json!({ "id": id, "secret": 1 }));
    let res = server.request(Method::GET, &format!("/people/{}?fields=address.zip", id), None, &[])?;
    assert_eq!(res.body, json!({ "id": id, "address": { "zip": "N1" } }));
    let gql = server.request(
        Method::POST,
        "/graphql",
        Some(&json!({ "query": "{ documents(tessellation: \"people\") { documents { data(fields: [\"name\"]) } } }" })),
        &[],
    )?;
    assert_eq!(gql.body["data"]["documents"]["documents"][0]["data"], json!({ "name": "Ada" }), "{}", gql.body);
    Ok(())
}

#[test]
fn metrics_history_survives_a_restart() -> Result<()> {
    let mut server = TestServer::start()?;
    server.insert("items", &json!({ "a": 1 }))?;
    let before = server.request(Method::GET, "/status/history?minutes=60", None, &[])?.body;
    let first = before["samples"][0]["timestamp"].clone();
    assert!(first.is_string(), "{}", before);
    server.restart()?;
    let after = server.request(Method::GET, "/status/history?minutes=60", None, &[])?.body;
    assert_eq!(after["samples"][0]["timestamp"], first, "older samples were reloaded");
    assert!(after["samples"].as_array().unwrap().len() >= 2);
    // Counters continue instead of restarting at zero.
    let samples = after["samples"].as_array().unwrap();
    let writes: Vec<u64> = samples.iter().map(|s| s["writes_total"].as_u64().unwrap()).collect();
    assert!(writes.windows(2).all(|w| w[1] >= w[0]), "{:?}", writes);
    assert!(!walk_contains(&server.data_dir(), b"writes_total"), "the history file is encrypted");
    Ok(())
}

fn walk_contains(dir: &std::path::Path, needle: &[u8]) -> bool {
    std::fs::read_dir(dir).map(|entries| {
        entries.flatten().any(|e| {
            let p = e.path();
            if p.is_dir() { walk_contains(&p, needle) } else { std::fs::read(&p).map(|b| b.windows(needle.len()).any(|w| w == needle)).unwrap_or(false) }
        })
    }).unwrap_or(false)
}
