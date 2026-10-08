//! Functions (saved queries, aggregations, transactions and scripts) and schedules.

use anyhow::{bail, Result};
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn run(server: &TestServer, token: Option<&str>, name: &str, params: Value) -> Result<hexdb_tests::ApiResponse> {
    let path = format!("/functions/{}/run", name);
    let body = json!({ "params": params });
    match token {
        Some(t) => server.request_as(Some(t), Method::POST, &path, Some(&body), &[]),
        None => server.request(Method::POST, &path, Some(&body), &[]),
    }
}

fn create(server: &TestServer, def: Value) -> Result<()> {
    let res = server.request(Method::POST, "/functions", Some(&def), &[])?;
    if res.status != 201 {
        bail!("creating {} returned {}: {}", def["name"], res.status, res.body);
    }
    Ok(())
}

#[test]
fn saved_functions_run_with_the_callers_permissions() -> Result<()> {
    let server = TestServer::start()?;
    for (status, total) in [("paid", 10), ("paid", 20), ("new", 5)] {
        server.insert("orders", &json!({ "status": status, "total": total }))?;
    }
    create(&server, json!({
        "name": "orders_by_status", "kind": "query", "tessellation": "orders",
        "params": [{ "name": "status", "type": "string", "required": true }],
        "body": { "filter": { "status": { "$param": "status" } }, "sort": "-total", "fields": ["total"] },
    }))?;
    create(&server, json!({
        "name": "revenue", "kind": "aggregate", "tessellation": "orders",
        "body": { "group_by": ["status"], "aggregates": { "sum": { "$sum": "total" } } },
    }))?;
    create(&server, json!({
        "name": "add_order", "kind": "transaction",
        "params": [{ "name": "total", "type": "number", "required": true }],
        "body": { "operations": [{ "op": "insert", "tessellation": "orders", "data": { "status": "new", "total": { "$param": "total" } } }] },
    }))?;

    // Validation of definitions and arguments.
    assert_eq!(server.request(Method::POST, "/functions", Some(&json!({ "name": "x", "kind": "query" })), &[])?.status, 400);
    assert_eq!(run(&server, None, "orders_by_status", json!({}))?.status, 400, "a required parameter");
    assert_eq!(run(&server, None, "orders_by_status", json!({ "status": 1 }))?.status, 400, "a typed parameter");

    let res = run(&server, None, "orders_by_status", json!({ "status": "paid" }))?;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(res.body["result"]["total"], 2);
    assert_eq!(res.body["result"]["documents"][0]["total"], 20);
    assert!(res.body["result"]["documents"][0].get("status").is_none(), "projected");
    let revenue = run(&server, None, "revenue", json!({}))?;
    assert!(revenue.body.to_string().contains("30"), "{}", revenue.body);
    assert_eq!(run(&server, None, "add_order", json!({ "total": 7 }))?.status, 200);
    assert_eq!(server.count("orders")?, 4);

    // A reader runs read functions, but not ones that write.
    let reader = server.user_with_roles("rita", json!([{ "name": "reader", "tessellations": ["orders"] }]))?;
    assert_eq!(run(&server, Some(&reader), "orders_by_status", json!({ "status": "new" }))?.status, 200);
    assert_eq!(run(&server, Some(&reader), "add_order", json!({ "total": 1 }))?.status, 403);
    // Only administrators define functions.
    assert_eq!(server.request_as(Some(&reader), Method::DELETE, "/functions/revenue", None, &[])?.status, 403);
    Ok(())
}

#[test]
fn scripts_run_in_python_and_typescript() -> Result<()> {
    let server = TestServer::start()?;
    server.insert("notes", &json!({ "n": 1 }))?;
    create(&server, json!({
        "name": "count_notes", "kind": "script", "runtime": "python",
        "params": [{ "name": "label", "type": "string", "default": "notes" }],
        "code": concat!(
            "import json, os, sys, urllib.request\n",
            "data = json.load(sys.stdin)\n",
            "req = urllib.request.Request(os.environ['HEXDB_API'] + '/notes/count', headers={'Authorization': 'Bearer ' + os.environ['HEXDB_TOKEN']})\n",
            "count = json.load(urllib.request.urlopen(req))['count']\n",
            "print(json.dumps({'label': data['params']['label'], 'count': count, 'caller': data['caller'], 'token': os.environ['HEXDB_TOKEN']}))\n",
        ),
    }))?;
    let res = run(&server, None, "count_notes", json!({}))?;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(res.body["result"]["count"], 1, "{}", res.body);
    assert_eq!(res.body["result"]["label"], "notes");
    assert_eq!(res.body["result"]["caller"], "admin");
    // The script's session ends with it.
    let token = res.body["result"]["token"].as_str().unwrap();
    assert_eq!(server.request_as(Some(token), Method::GET, "/notes/count", None, &[])?.status, 401);

    // Timeouts are reported.
    create(&server, json!({ "name": "slow", "kind": "script", "runtime": "python", "timeout_seconds": 1, "code": "import time\ntime.sleep(10)" }))?;
    let started = Instant::now();
    let res = run(&server, None, "slow", json!({}))?;
    assert_eq!(res.status, 400);
    assert!(res.body["error"]["message"].as_str().unwrap().contains("didn't finish"));
    assert!(started.elapsed() < Duration::from_secs(8));

    // TypeScript and JavaScript need Node.js (22.6 or later for TypeScript).
    if !std::process::Command::new("node").arg("--version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("Node.js not found; skipping the TypeScript and JavaScript scripts");
        return Ok(());
    }
    create(&server, json!({
        "name": "double", "kind": "script", "runtime": "typescript",
        "params": [{ "name": "n", "type": "number", "required": true }],
        "code": concat!(
            "interface Input { params: { n: number } }\n",
            "const chunks: Buffer[] = []\n",
            "for await (const chunk of process.stdin) chunks.push(chunk as Buffer)\n",
            "const input: Input = JSON.parse(Buffer.concat(chunks).toString())\n",
            "console.log(JSON.stringify({ doubled: input.params.n * 2 }))\n",
        ),
    }))?;
    let res = run(&server, None, "double", json!({ "n": 21 }))?;
    assert_eq!(res.body["result"]["doubled"], 42, "{}", res.body);

    // Failures are reported.
    create(&server, json!({ "name": "boom", "kind": "script", "runtime": "javascript", "code": "throw new Error('kaboom')" }))?;
    let res = run(&server, None, "boom", json!({}))?;
    assert_eq!(res.status, 400);
    Ok(())
}

#[test]
fn schedules_run_functions_on_time() -> Result<()> {
    let server = TestServer::start()?;
    create(&server, json!({
        "name": "tick", "kind": "transaction",
        "body": { "operations": [{ "op": "insert", "tessellation": "ticks", "data": { "at": "now" } }] },
    }))?;
    assert_eq!(server.request(Method::POST, "/schedules", Some(&json!({ "name": "bad", "function": "tick", "cron": "61 * * * *" })), &[])?.status, 400);
    assert_eq!(server.request(Method::POST, "/schedules", Some(&json!({ "name": "fast", "function": "tick", "every_seconds": 1 })), &[])?.status, 400);
    let res = server.request(Method::POST, "/schedules", Some(&json!({ "name": "ticker", "function": "tick", "every_seconds": 10 })), &[])?;
    assert_eq!(res.status, 201, "{}", res.body);
    assert_eq!(res.body["run_as_login"], "admin");

    // Run now by hand, then wait for the scheduler.
    assert_eq!(server.request(Method::POST, "/schedules/ticker/run", None, &[])?.status, 200);
    assert_eq!(server.count("ticks")?, 1);
    let deadline = Instant::now() + Duration::from_secs(20);
    while server.count("ticks").unwrap_or(0) < 2 {
        if Instant::now() > deadline {
            bail!("the schedule didn't run");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let schedule = server.request(Method::GET, "/schedules/ticker", None, &[])?;
    assert_eq!(schedule.body["last_status"], "ok", "{}", schedule.body);
    assert!(schedule.body["runs"].as_u64().unwrap() >= 2);

    // A function used by a schedule can't be deleted until the schedule is.
    assert_eq!(server.request(Method::DELETE, "/functions/tick", None, &[])?.status, 409);
    assert_eq!(server.request(Method::DELETE, "/schedules/ticker", None, &[])?.status, 204);
    assert_eq!(server.request(Method::DELETE, "/functions/tick", None, &[])?.status, 204);
    Ok(())
}
