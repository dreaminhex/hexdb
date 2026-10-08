//! Secondary indexes: creation, use by queries, maintenance on writes,
//! uniqueness, full-text search, persistence across restarts, and equivalence
//! with unindexed queries.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::{json, Value};

fn create_index(server: &TestServer, tess: &str, def: Value) -> Result<hexdb_tests::ApiResponse> {
    server.request(Method::POST, &format!("/tessellations/{}/indexes", tess), Some(&def), &[])
}

/// Run a query; returns (sorted IDs, indexes used, documents scanned).
fn query(server: &TestServer, tess: &str, filter: Value) -> Result<(Vec<String>, Vec<String>, u64)> {
    let res = server.request(Method::POST, &format!("/{}/_query", tess), Some(&json!({ "filter": filter, "limit": 1000 })), &[])?;
    assert_eq!(res.status.as_u16(), 200, "{}", res.body);
    let mut ids: Vec<String> = res.body["documents"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect();
    ids.sort();
    let used = res.body["plan"]["indexes"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    Ok((ids, used, res.body["plan"]["scanned"].as_u64().unwrap()))
}

fn seed(server: &TestServer, n: usize) -> Result<()> {
    let statuses = ["draft", "live", "archived"];
    let docs: Vec<Value> = (0..n)
        .map(|i| {
            let mut doc = json!({
                "n": i,
                "status": statuses[i % 3],
                "score": (i * 37 % 101) as f64 / 4.0,
                "author": { "name": format!("author{}", i % 7) },
                "tags": [format!("t{}", i % 5), format!("t{}", i % 4)],
                "title": format!("Post {} about {}", i, ["rust", "databases", "hexagons", "tessellation"][i % 4]),
            });
            if i % 10 == 0 {
                doc.as_object_mut().unwrap().remove("status");
            }
            if i % 13 == 0 {
                doc["score"] = json!("unknown");
            }
            doc
        })
        .collect();
    let res = server.request(Method::POST, "/posts/_bulk", Some(&Value::Array(docs)), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    Ok(())
}

fn filters() -> Vec<Value> {
    vec![
        json!({ "status": "live" }),
        json!({ "status": null }),
        json!({ "status": { "$in": ["draft", "archived"] } }),
        json!({ "score": { "$gt": 10 } }),
        json!({ "score": { "$gte": 5, "$lt": 12.5 } }),
        json!({ "score": { "$lte": 3 } }),
        json!({ "author.name": "author3" }),
        json!({ "author.name": { "$startsWith": "author1" } }),
        json!({ "tags": "t2" }),
        json!({ "status": "live", "score": { "$gt": 15 } }),
        json!({ "status": "draft", "author.name": "author2" }),
        json!({ "$or": [{ "status": "archived" }, { "tags": "t0" }] }),
        json!({ "$text": "rust" }),
        json!({ "$text": "post databases" }),
        json!({ "status": { "$ne": "live" } }),
        json!({ "$not": { "tags": "t1" } }),
    ]
}

#[test]
fn indexed_queries_return_exactly_what_scans_return() -> Result<()> {
    let server = TestServer::start()?;
    seed(&server, 300)?;
    let unindexed: Vec<Vec<String>> = filters().into_iter().map(|f| query(&server, "posts", f).map(|r| r.0)).collect::<Result<_>>()?;

    for def in [
        json!({ "fields": ["status"] }),
        json!({ "fields": ["score"] }),
        json!({ "fields": ["author.name"] }),
        json!({ "fields": ["tags"] }),
        json!({ "fields": ["status", "author.name"], "name": "status_author" }),
        json!({ "fields": ["title"], "kind": "text" }),
    ] {
        let res = create_index(&server, "posts", def.clone())?;
        assert_eq!(res.status.as_u16(), 201, "{} -> {}", def, res.body);
    }
    for (filter, expected) in filters().into_iter().zip(&unindexed) {
        let (ids, used, scanned) = query(&server, "posts", filter.clone())?;
        assert_eq!(&ids, expected, "{} returned different documents with indexes", filter);
        let negated = filter.get("$not").is_some() || filter.to_string().contains("$ne");
        if negated {
            assert!(used.is_empty(), "{} can't use an index", filter);
        } else {
            assert!(!used.is_empty(), "{} should use an index", filter);
            assert!(scanned < 300, "{} scanned {}", filter, scanned);
        }
    }
    let (_, used, scanned) = query(&server, "posts", json!({ "status": "draft", "author.name": "author2" }))?;
    assert!(used.contains(&"status_author".to_string()), "{:?}", used);
    assert!(scanned <= 15, "composite lookup is exact; scanned {}", scanned);

    // Writes keep indexes current.
    let id = server.insert("posts", &json!({ "status": "brand-new", "score": 999, "title": "zebra" }))?;
    assert_eq!(query(&server, "posts", json!({ "status": "brand-new" }))?.0, std::slice::from_ref(&id));
    assert_eq!(query(&server, "posts", json!({ "$text": "zebra" }))?.0, std::slice::from_ref(&id));
    server.patch("posts", &json!({ "id": id, "status": "changed", "title": "giraffe" }))?;
    assert!(query(&server, "posts", json!({ "status": "brand-new" }))?.0.is_empty());
    assert!(query(&server, "posts", json!({ "$text": "zebra" }))?.0.is_empty());
    assert_eq!(query(&server, "posts", json!({ "status": "changed" }))?.0, std::slice::from_ref(&id));
    server.delete("posts", &id)?;
    assert!(query(&server, "posts", json!({ "status": "changed" }))?.0.is_empty());

    // Update by filter uses indexes too and modifies exactly the matches.
    let res = server.request(Method::POST, "/posts/_update", Some(&json!({ "filter": { "status": "archived" }, "update": { "flag": true } })), &[])?;
    assert_eq!(res.status.as_u16(), 200, "{}", res.body);
    let archived = query(&server, "posts", json!({ "status": "archived" }))?.0;
    assert_eq!(res.body["modified"].as_u64().unwrap() as usize, archived.len());
    assert_eq!(query(&server, "posts", json!({ "flag": true }))?.0, archived);

    // Counts and aggregations use indexes too, and agree with queries.
    let count = server.request(Method::GET, "/posts/count?filter=%7B%22status%22%3A%22live%22%7D", None, &[])?;
    assert_eq!(count.body["count"].as_u64().unwrap() as usize, unindexed[0].len());
    let agg = server.request(Method::POST, "/posts/_aggregate", Some(&json!({ "filter": { "$text": "rust" } })), &[])?;
    assert_eq!(agg.body["rows"][0]["count"].as_u64().unwrap() as usize, unindexed[12].len());
    Ok(())
}

#[test]
fn unique_indexes_reject_duplicates() -> Result<()> {
    let server = TestServer::start()?;
    let a = server.insert("accounts", &json!({ "email": "a@x.io" }))?;
    server.insert("accounts", &json!({ "nickname": "no email" }))?;
    server.insert("accounts", &json!({ "nickname": "also none" }))?;
    let res = create_index(&server, "accounts", json!({ "fields": ["email"], "unique": true }))?;
    assert_eq!(res.status.as_u16(), 201, "{}", res.body);

    let dup = server.request(Method::POST, "/accounts", Some(&json!({ "email": "a@x.io" })), &[])?;
    assert_eq!(dup.status.as_u16(), 409, "{}", dup.body);
    assert!(dup.body["error"]["message"].as_str().unwrap().contains("email"));

    let b = server.insert("accounts", &json!({ "email": "b@x.io" }))?;
    let res = server.request(Method::PATCH, &format!("/accounts/{}", b), Some(&json!({ "email": "a@x.io" })), &[])?;
    assert_eq!(res.status.as_u16(), 409);
    let res = server.request(Method::POST, "/accounts/_bulk", Some(&json!([{ "email": "c@x.io" }, { "email": "c@x.io" }])), &[])?;
    assert_eq!(res.status.as_u16(), 409, "duplicates within one request");
    assert_eq!(server.count("accounts")?, 4);

    // Swapping values in one transaction is fine: the check sees the final state.
    let res = server.request(
        Method::POST,
        "/transactions",
        Some(&json!({ "operations": [
            { "op": "patch", "tessellation": "accounts", "id": a, "data": { "email": "b@x.io" } },
            { "op": "patch", "tessellation": "accounts", "id": b, "data": { "email": "a@x.io" } },
        ]})),
        &[],
    )?;
    assert_eq!(res.status.as_u16(), 200, "{}", res.body);

    // A unique index can't be created over duplicates, and leaves nothing behind.
    server.insert("people", &json!({ "name": "x" }))?;
    server.insert("people", &json!({ "name": "x" }))?;
    let res = create_index(&server, "people", json!({ "fields": ["name"], "unique": true }))?;
    assert_eq!(res.status.as_u16(), 409, "{}", res.body);
    let list = server.request(Method::GET, "/tessellations/people/indexes", None, &[])?;
    assert_eq!(list.body["indexes"], json!([]));
    Ok(())
}

#[test]
fn indexes_survive_restarts_and_can_be_dropped() -> Result<()> {
    let mut server = TestServer::start()?;
    seed(&server, 60)?;
    assert_eq!(create_index(&server, "posts", json!({ "fields": ["status"] }))?.status.as_u16(), 201);
    assert_eq!(create_index(&server, "posts", json!({ "fields": ["title"], "kind": "text" }))?.status.as_u16(), 201);

    // Duplicates and bad definitions are rejected.
    assert_eq!(create_index(&server, "posts", json!({ "fields": ["status"] }))?.status.as_u16(), 409);
    assert_eq!(create_index(&server, "posts", json!({ "fields": ["body"], "kind": "text" }))?.status.as_u16(), 409);
    assert_eq!(create_index(&server, "posts", json!({ "fields": [] }))?.status.as_u16(), 400);
    assert_eq!(create_index(&server, "users", json!({ "fields": ["login"] }))?.status.as_u16(), 403);
    assert_eq!(create_index(&server, "nothing", json!({ "fields": ["a"] }))?.status.as_u16(), 404);

    let before = query(&server, "posts", json!({ "status": "live" }))?;
    server.crash_and_restart()?;
    let list = server.request(Method::GET, "/tessellations/posts/indexes", None, &[])?;
    let indexes = list.body["indexes"].as_array().unwrap();
    assert_eq!(indexes.len(), 2, "{}", list.body);
    assert!(indexes.iter().all(|i| i["ready"] == true && i["documents"] == 60), "{}", list.body);
    let after = query(&server, "posts", json!({ "status": "live" }))?;
    assert_eq!(before.0, after.0);
    assert_eq!(after.1, ["status"]);

    let res = server.request(Method::DELETE, "/tessellations/posts/indexes/status", None, &[])?;
    assert_eq!(res.status.as_u16(), 204);
    let res = server.request(Method::DELETE, "/tessellations/posts/indexes/status", None, &[])?;
    assert_eq!(res.status.as_u16(), 404);
    let (ids, used, scanned) = query(&server, "posts", json!({ "status": "live" }))?;
    assert_eq!(ids, before.0);
    assert!(used.is_empty());
    assert_eq!(scanned, 60);
    server.restart()?;
    let list = server.request(Method::GET, "/tessellations/posts/indexes", None, &[])?;
    assert_eq!(list.body["indexes"].as_array().unwrap().len(), 1, "the drop persisted");
    Ok(())
}

#[test]
fn indexes_over_graphql() -> Result<()> {
    let server = TestServer::start()?;
    seed(&server, 30)?;
    let create = r#"mutation { createIndex(tessellation: "posts", fields: ["status"]) { name kind fields unique documents ready } }"#;
    let res = server.request(Method::POST, "/graphql", Some(&json!({ "query": create })), &[])?;
    assert!(res.body["errors"].is_null(), "{}", res.body);
    assert_eq!(res.body["data"]["createIndex"]["documents"], 30);

    let q = r#"{ documents(tessellation: "posts", filter: { status: "live" }) { total indexesUsed scanned } tessellation(name: "posts") { indexes { name } } }"#;
    let res = server.request(Method::POST, "/graphql", Some(&json!({ "query": q })), &[])?;
    assert!(res.body["errors"].is_null(), "{}", res.body);
    assert_eq!(res.body["data"]["documents"]["indexesUsed"], json!(["status"]));
    assert_eq!(res.body["data"]["tessellation"]["indexes"], json!([{ "name": "status" }]));

    let drop = r#"mutation { dropIndex(tessellation: "posts", name: "status") }"#;
    let res = server.request(Method::POST, "/graphql", Some(&json!({ "query": drop })), &[])?;
    assert_eq!(res.body["data"]["dropIndex"], true, "{}", res.body);
    Ok(())
}

#[test]
fn index_snapshots_load_at_startup_and_catch_up_after_a_crash() -> Result<()> {
    let mut server = TestServer::start()?;
    seed(&server, 300)?;
    assert_eq!(create_index(&server, "posts", json!({ "fields": ["status"] }))?.status, 201);
    assert_eq!(create_index(&server, "posts", json!({ "kind": "text", "fields": ["title"] }))?.status, 201);
    let before_status = query(&server, "posts", json!({ "status": "live" }))?;
    let before_text = query(&server, "posts", json!({ "$text": "hexagons" }))?;

    // A clean restart loads the snapshots instead of rebuilding.
    server.restart()?;
    assert!(server.log_tail(80).contains("Loaded index 'status' on 'posts'"), "{}", server.log_tail(80));
    assert_eq!(query(&server, "posts", json!({ "status": "live" }))?, before_status);
    assert_eq!(query(&server, "posts", json!({ "$text": "hexagons" }))?, before_text);

    // Writes after the snapshot, then a crash: the snapshot is brought up to date.
    let added = server.insert("posts", &json!({ "status": "live", "title": "fresh hexagons" }))?;
    let (live_ids, _, _) = query(&server, "posts", json!({ "status": "live" }))?;
    let gone = live_ids.iter().find(|id| **id != added).unwrap().clone();
    server.delete("posts", &gone)?;
    let changed = live_ids[0].clone();
    if changed != gone && changed != added {
        server.patch("posts", &json!({ "id": changed, "status": "archived" }))?;
    }
    let expected_status = query(&server, "posts", json!({ "status": "live" }))?;
    let expected_text = query(&server, "posts", json!({ "$text": "hexagons" }))?;
    server.crash_and_restart()?;
    assert!(server.log_tail(80).contains("updated since its snapshot"), "{}", server.log_tail(80));
    assert_eq!(query(&server, "posts", json!({ "status": "live" }))?, expected_status);
    assert_eq!(query(&server, "posts", json!({ "$text": "hexagons" }))?, expected_text);
    assert!(expected_status.0.contains(&added) && !expected_status.0.contains(&gone));

    // A damaged snapshot is rebuilt from the documents.
    server.stop()?;
    std::fs::write(server.data_dir().join("indexes").join("posts").join("status.hxi"), b"garbage")?;
    server.launch()?;
    assert!(server.log_tail(80).contains("Rebuilt index 'status' on 'posts'"), "{}", server.log_tail(80));
    assert_eq!(query(&server, "posts", json!({ "status": "live" }))?, expected_status);
    Ok(())
}

/// One page of a sorted query: the IDs in order, `total`, and the indexes used.
fn sorted_page(server: &TestServer, tess: &str, body: &Value) -> Result<(Vec<String>, Value, Vec<String>)> {
    let res = server.request(Method::POST, &format!("/{}/_query", tess), Some(body), &[])?;
    assert_eq!(res.status.as_u16(), 200, "{} -> {}", body, res.body);
    let ids = res.body["documents"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect();
    let used = res.body["plan"]["indexes"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    Ok((ids, res.body["total"].clone(), used))
}

#[test]
fn sorted_queries_walk_the_sort_index_and_match_in_memory_sorting() -> Result<()> {
    let server = TestServer::start()?;
    // Ranks repeat (ties), some are missing or null, and groups give filters something to do.
    let docs: Vec<Value> = (0..240)
        .map(|i| {
            let group = ["a", "b", "c"][i % 3];
            let mut doc = json!({ "n": i, "rank": (i * 7) % 23, "group": group });
            if i % 17 == 0 {
                doc.as_object_mut().unwrap().remove("rank");
            }
            if i % 29 == 0 {
                doc["rank"] = Value::Null;
            }
            doc
        })
        .collect();
    assert_eq!(server.request(Method::POST, "/ranked/_bulk", Some(&json!(docs)), &[])?.status.as_u16(), 201);
    assert_eq!(create_index(&server, "ranked", json!({ "fields": ["group"] }))?.status.as_u16(), 201);

    let mut queries = Vec::new();
    for filter in [json!({}), json!({ "group": "b" }), json!({ "n": { "$gte": 100 } }), json!({ "group": { "$in": ["a", "c"] }, "n": { "$lt": 200 } })] {
        for sort in ["rank", "-rank", "rank,-n", "-rank,group,n"] {
            for (offset, limit) in [(0, 10), (5, 20), (60, 25), (230, 50)] {
                queries.push(json!({ "filter": filter, "sort": sort, "offset": offset, "limit": limit }));
            }
        }
    }

    // Before the index exists every query sorts in memory: the reference answers.
    let mut expected = Vec::new();
    for q in &queries {
        expected.push(sorted_page(&server, "ranked", q)?);
    }
    assert_eq!(create_index(&server, "ranked", json!({ "fields": ["rank"] }))?.status.as_u16(), 201);

    for (q, (ids, total, _)) in queries.iter().zip(&expected) {
        // With the total: the same page and total (filtered queries still sort in memory).
        let (with_total, counted, _) = sorted_page(&server, "ranked", q)?;
        assert_eq!(&with_total, ids, "{}", q);
        assert_eq!(&counted, total, "{}", q);
        // Without it: the same page, read through the sort index, and no total.
        let mut fast = q.clone();
        fast["total"] = json!(false);
        let (page, no_total, used) = sorted_page(&server, "ranked", &fast)?;
        assert_eq!(&page, ids, "{}", fast);
        assert!(no_total.is_null(), "{}: total {}", fast, no_total);
        assert!(used.contains(&"rank".to_string()), "{} used {:?}", fast, used);
    }

    // Unsorted, filtered, without a total: the same page in ID order, read only as far as needed.
    let all = sorted_page(&server, "ranked", &json!({ "filter": { "group": "c" }, "limit": 1000 }))?.0;
    let res = server.request(Method::POST, "/ranked/_query", Some(&json!({ "filter": { "group": "c" }, "limit": 10, "total": false })), &[])?;
    let page: Vec<String> = res.body["documents"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(page, all[..10].to_vec());
    assert!(res.body["total"].is_null());
    assert_eq!(res.body["next"], json!(all[9]));
    assert!(res.body["plan"]["scanned"].as_u64().unwrap() <= 11, "{}", res.body);
    Ok(())
}
