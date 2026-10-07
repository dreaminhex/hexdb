//! Transactions: atomic multi-document, multi-tessellation writes with
//! preconditions, rollback, idempotency, and optimistic concurrency.

use anyhow::Result;
use hexdb_tests::{ApiResponse, TestServer};
use reqwest::Method;
use serde_json::{json, Value};

fn tx(server: &TestServer, ops: Value) -> Result<ApiResponse> {
    server.request(Method::POST, "/transactions", Some(&json!({ "operations": ops })), &[])
}

fn etag(server: &TestServer, tess: &str, id: &str) -> Result<u64> {
    let res = server.request(Method::GET, &format!("/{}/{}", tess, id), None, &[])?;
    let tag = res.headers.get("etag").expect("ETag header").to_str()?.trim_matches('"').to_string();
    Ok(tag.parse()?)
}

fn balance(server: &TestServer, id: &str) -> Result<i64> {
    Ok(server.get_doc("accounts", id)?.expect("account exists")["balance"].as_i64().unwrap())
}

#[test]
fn transfer_commits_atomically_and_reports_versions() -> Result<()> {
    let server = TestServer::start()?;
    let a = server.insert("accounts", &json!({ "owner": "ada", "balance": 100 }))?;
    let b = server.insert("accounts", &json!({ "owner": "bo", "balance": 5 }))?;
    server.insert("ledger", &json!({ "seed": true }))?;

    let res = tx(
        &server,
        json!([
            { "op": "get", "tessellation": "accounts", "id": a },
            { "op": "patch", "tessellation": "accounts", "id": a, "data": { "balance": 70 }, "if_match": { "balance": { "$gte": 30 } } },
            { "op": "patch", "tessellation": "accounts", "id": b, "data": { "balance": 35 } },
            { "op": "insert", "tessellation": "ledger", "data": { "from": a, "to": b, "amount": 30 } },
        ]),
    )?;
    assert_eq!(res.status.as_u16(), 200, "{}", res.body);
    assert_eq!(res.body["writes"], 3);
    let results = res.body["results"].as_array().unwrap();
    assert_eq!(results[0]["document"]["balance"], 100, "get sees the state before the patch");
    assert_eq!(results[1]["document"]["balance"], 70);
    assert_eq!(balance(&server, &a)?, 70);
    assert_eq!(balance(&server, &b)?, 35);
    assert_eq!(server.count("ledger")?, 2);

    // Versions in the results match what reads report, and are consecutive.
    let va = results[1]["version"].as_u64().unwrap();
    let vb = results[2]["version"].as_u64().unwrap();
    assert_eq!(etag(&server, "accounts", &a)?, va);
    assert_eq!(etag(&server, "accounts", &b)?, vb);
    assert_eq!(results[0]["version"], va, "results for one document report its final version");
    assert_eq!(vb, va + 1);

    // The whole transaction survives a crash.
    let mut server = server;
    server.crash_and_restart()?;
    assert_eq!(balance(&server, &a)?, 70);
    assert_eq!(balance(&server, &b)?, 35);
    assert_eq!(server.count("ledger")?, 2);
    Ok(())
}

#[test]
fn any_failure_rolls_back_everything() -> Result<()> {
    let server = TestServer::start()?;
    let a = server.insert("accounts", &json!({ "balance": 10 }))?;
    let b = server.insert("accounts", &json!({ "balance": 0 }))?;
    server.insert("ledger", &json!({ "seed": true }))?;

    // if_match fails on the last operation: nothing before it is written.
    let res = tx(
        &server,
        json!([
            { "op": "patch", "tessellation": "accounts", "id": b, "data": { "balance": 50 } },
            { "op": "insert", "tessellation": "ledger", "data": { "amount": 50 } },
            { "op": "patch", "tessellation": "accounts", "id": a, "data": { "balance": -40 }, "if_match": { "balance": { "$gte": 50 } } },
        ]),
    )?;
    assert_eq!(res.status.as_u16(), 409, "{}", res.body);
    assert!(res.body["error"]["message"].as_str().unwrap().contains("operations[2]"), "{}", res.body);
    assert_eq!(balance(&server, &a)?, 10);
    assert_eq!(balance(&server, &b)?, 0);
    assert_eq!(server.count("ledger")?, 1);

    // A missing document is a 404, and also rolls back.
    let res = tx(
        &server,
        json!([
            { "op": "insert", "tessellation": "ledger", "data": { "amount": 1 } },
            { "op": "delete", "tessellation": "accounts", "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV" },
        ]),
    )?;
    assert_eq!(res.status.as_u16(), 404, "{}", res.body);
    assert_eq!(server.count("ledger")?, 1);

    // A stale version is a conflict.
    let version = etag(&server, "accounts", &a)?;
    server.patch("accounts", &json!({ "id": a, "note": "changed" }))?;
    let res = tx(&server, json!([{ "op": "replace", "tessellation": "accounts", "id": a, "data": { "balance": 1 }, "if_version": version }]))?;
    assert_eq!(res.status.as_u16(), 409, "{}", res.body);
    assert_eq!(balance(&server, &a)?, 10);

    // Malformed requests and system tessellations are rejected up front.
    for bad in [
        json!([{ "op": "upsert", "tessellation": "accounts", "id": a }]),
        json!([{ "op": "patch", "tessellation": "accounts", "data": {} }]),
        json!([{ "op": "insert", "tessellation": "accounts" }]),
        json!([{ "op": "get", "tessellation": "users", "id": a }]),
        json!([]),
    ] {
        let res = tx(&server, bad.clone())?;
        assert_eq!(res.status.as_u16(), 400, "{} -> {}", bad, res.body);
    }
    let res = tx(&server, json!([{ "op": "get", "tessellation": "nowhere", "id": a }]))?;
    assert_eq!(res.status.as_u16(), 404);
    Ok(())
}

#[test]
fn operations_see_earlier_operations() -> Result<()> {
    let server = TestServer::start()?;
    server.insert("things", &json!({ "seed": true }))?;
    let id = "01HZY0000000000000000000AA";

    let res = tx(
        &server,
        json!([
            { "op": "check", "tessellation": "things", "id": id, "if_version": 0 },
            { "op": "insert", "tessellation": "things", "id": id, "data": { "n": 1, "keep": true } },
            { "op": "patch", "tessellation": "things", "id": id, "data": { "n": 2, "keep": null } },
            { "op": "get", "tessellation": "things", "id": id },
        ]),
    )?;
    assert_eq!(res.status.as_u16(), 200, "{}", res.body);
    assert_eq!(res.body["writes"], 1, "one write per document");
    assert_eq!(res.body["results"][3]["document"], json!({ "id": id, "n": 2 }));
    assert_eq!(server.get_doc("things", id)?.unwrap(), json!({ "id": id, "n": 2 }));

    // Inserting the same ID again conflicts.
    let res = tx(&server, json!([{ "op": "insert", "tessellation": "things", "id": id, "data": {} }]))?;
    assert_eq!(res.status.as_u16(), 409);

    // Insert then delete in one transaction writes nothing.
    let other = "01HZY0000000000000000000BB";
    let res = tx(
        &server,
        json!([
            { "op": "insert", "tessellation": "things", "id": other, "data": { "temp": true } },
            { "op": "delete", "tessellation": "things", "id": other },
        ]),
    )?;
    assert_eq!(res.status.as_u16(), 200, "{}", res.body);
    assert_eq!(res.body["writes"], 0);
    assert!(server.get_doc("things", other)?.is_none());
    Ok(())
}

#[test]
fn idempotent_transactions_replay() -> Result<()> {
    let server = TestServer::start()?;
    let a = server.insert("accounts", &json!({ "balance": 1 }))?;
    let body = json!({ "operations": [
        { "op": "patch", "tessellation": "accounts", "id": a, "data": { "balance": 2 } },
        { "op": "insert", "tessellation": "accounts", "data": { "balance": 0 } },
    ]});
    let headers = [("Idempotency-Key", "transfer-1")];
    let first = server.request(Method::POST, "/transactions", Some(&body), &headers)?;
    let second = server.request(Method::POST, "/transactions", Some(&body), &headers)?;
    assert_eq!(first.status.as_u16(), 200, "{}", first.body);
    assert!(!first.replayed());
    assert!(second.replayed());
    assert_eq!(first.body, second.body);
    assert_eq!(server.count("accounts")?, 2);
    assert_eq!(etag(&server, "accounts", &a)?, first.body["results"][0]["version"].as_u64().unwrap());

    let different = json!({ "operations": [{ "op": "get", "tessellation": "accounts", "id": a }] });
    let res = server.request(Method::POST, "/transactions", Some(&different), &headers)?;
    assert_eq!(res.status.as_u16(), 422);
    Ok(())
}

#[test]
fn concurrent_read_modify_write_loses_no_updates() -> Result<()> {
    let server = TestServer::start()?;
    let counter = server.insert("counters", &json!({ "n": 0 }))?;
    let (threads, per_thread) = (6, 10);

    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                for _ in 0..per_thread {
                    loop {
                        // Read with its version, then write conditionally; retry on conflict.
                        let read = tx(&server, json!([{ "op": "get", "tessellation": "counters", "id": counter }])).unwrap();
                        let n = read.body["results"][0]["document"]["n"].as_i64().unwrap();
                        let version = read.body["results"][0]["version"].as_u64().unwrap();
                        let write = tx(
                            &server,
                            json!([{ "op": "patch", "tessellation": "counters", "id": counter, "data": { "n": n + 1 }, "if_version": version }]),
                        )
                        .unwrap();
                        match write.status.as_u16() {
                            200 => break,
                            409 => continue,
                            other => panic!("unexpected {}: {}", other, write.body),
                        }
                    }
                }
            });
        }
    });
    assert_eq!(server.get_doc("counters", &counter)?.unwrap()["n"], threads * per_thread);
    Ok(())
}

#[test]
fn transactions_over_graphql() -> Result<()> {
    let server = TestServer::start()?;
    let a = server.insert("accounts", &json!({ "balance": 9 }))?;
    let query = r#"mutation($ops: JSON!) { transaction(operations: $ops, idempotencyKey: "g1") { writes results { op id version document } } }"#;
    let ops = json!([
        { "op": "patch", "tessellation": "accounts", "id": a, "data": { "balance": 4 }, "if_match": { "balance": { "_gt": 5 } } },
        { "op": "insert", "tessellation": "accounts", "data": { "balance": 5 } },
    ]);
    let res = server.request(Method::POST, "/graphql", Some(&json!({ "query": query, "variables": { "ops": ops } })), &[])?;
    assert!(res.body["errors"].is_null(), "{}", res.body);
    let data = &res.body["data"]["transaction"];
    assert_eq!(data["writes"], 2);
    assert_eq!(data["results"][0]["op"], "patch");
    assert_eq!(data["results"][0]["document"]["balance"], 4);
    assert_eq!(balance(&server, &a)?, 4);

    // The precondition now fails: CONFLICT, nothing written.
    let again = json!([
        { "op": "insert", "tessellation": "accounts", "data": { "balance": 1 } },
        { "op": "patch", "tessellation": "accounts", "id": a, "data": { "balance": 0 }, "if_match": { "balance": { "_gt": 5 } } },
    ]);
    let query = r#"mutation($ops: JSON!) { transaction(operations: $ops) { writes } }"#;
    let res = server.request(Method::POST, "/graphql", Some(&json!({ "query": query, "variables": { "ops": again } })), &[])?;
    assert_eq!(res.body["errors"][0]["extensions"]["code"], "CONFLICT", "{}", res.body);
    assert_eq!(server.count("accounts")?, 2);
    Ok(())
}
