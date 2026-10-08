//! Triggers: before triggers that change or refuse writes, after triggers that
//! run functions on committed changes, and no cascades.

use anyhow::{bail, Result};
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn wait_until(timeout: Duration, what: &str, mut check: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check()? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!("timed out waiting for {}", what)
}

fn created(server: &TestServer, path: &str, body: Value) -> Result<()> {
    let res = server.request(Method::POST, path, Some(&body), &[])?;
    assert_eq!(res.status.as_u16(), 201, "POST {} {} -> {}", path, body, res.body);
    Ok(())
}

fn log_entries(server: &TestServer) -> Result<Vec<Value>> {
    let res = server.request(Method::POST, "/order_log/_query", Some(&json!({ "limit": 100 })), &[])?;
    Ok(res.body["documents"].as_array().cloned().unwrap_or_default())
}

#[test]
fn before_triggers_change_or_refuse_writes_and_after_triggers_run_functions() -> Result<()> {
    let server = TestServer::start()?;

    // A before trigger: refuse negative totals, stamp the rest.
    created(&server, "/functions", json!({
        "name": "check_order", "kind": "script", "runtime": "python",
        "code": concat!(
            "import json, sys\n",
            "t = json.load(sys.stdin)['trigger']\n",
            "doc = t['document']\n",
            "if doc.get('total', 0) < 0:\n",
            "    print(json.dumps({'reject': 'totals can not be negative'}))\n",
            "else:\n",
            "    doc['checked'] = t['event']\n",
            "    doc['by'] = t['user']\n",
            "    print(json.dumps({'document': doc}))\n",
        ),
    }))?;
    created(&server, "/triggers", json!({ "name": "check-orders", "tessellation": "orders", "events": ["insert", "update"], "timing": "before", "function": "check_order" }))?;

    let bad = server.request(Method::POST, "/orders", Some(&json!({ "total": -1 })), &[])?;
    assert_eq!(bad.status.as_u16(), 422, "{}", bad.body);
    assert_eq!(bad.body["error"]["code"], "trigger_rejected");
    assert!(bad.body["error"]["message"].as_str().unwrap().contains("can not be negative"));
    let good = server.request(Method::POST, "/orders", Some(&json!({ "total": 5 })), &[])?;
    assert_eq!(good.status.as_u16(), 201, "{}", good.body);
    assert_eq!(good.body["checked"], "insert", "the response shows what was stored: {}", good.body);
    let id = good.body["id"].as_str().unwrap().to_string();
    let stored = server.get_doc("orders", &id)?.unwrap();
    assert_eq!((stored["checked"].clone(), stored["by"].clone()), (json!("insert"), json!("admin")));
    let patched = server.request(Method::PATCH, &format!("/orders/{}", id), Some(&json!({ "total": 7 })), &[])?;
    assert_eq!(patched.body["checked"], "update", "{}", patched.body);
    assert_eq!(server.request(Method::PATCH, &format!("/orders/{}", id), Some(&json!({ "total": -7 })), &[])?.status.as_u16(), 422);
    // Bulk writes and transactions go through before triggers too.
    assert_eq!(server.request(Method::POST, "/orders/_bulk", Some(&json!([{ "total": 1 }, { "total": -2 }])), &[])?.status.as_u16(), 422);
    let tx = server.request(Method::POST, "/transactions", Some(&json!({ "operations": [{ "op": "insert", "tessellation": "orders", "data": { "total": -3 } }] })), &[])?;
    assert_eq!(tx.status.as_u16(), 422, "{}", tx.body);

    // A before trigger must be a script.
    created(&server, "/functions", json!({
        "name": "log_order", "kind": "transaction",
        "params": [{ "name": "id", "type": "string", "required": true }, { "name": "event", "type": "string", "required": true }],
        "body": { "operations": [{ "op": "insert", "tessellation": "order_log", "data": { "order": { "$param": "id" }, "event": { "$param": "event" } } }] },
    }))?;
    let res = server.request(Method::POST, "/triggers", Some(&json!({ "name": "wrong", "tessellation": "orders", "timing": "before", "function": "log_order" })), &[])?;
    assert_eq!(res.status.as_u16(), 400, "{}", res.body);

    // An after trigger: log inserts and deletes.
    created(&server, "/triggers", json!({ "name": "log-orders", "tessellation": "orders", "events": ["insert", "delete"], "function": "log_order" }))?;
    // A before trigger on the log that refuses everything: trigger writes don't fire triggers.
    created(&server, "/functions", json!({ "name": "refuse", "kind": "script", "runtime": "python", "code": "import json\nprint(json.dumps({'reject': 'no'}))\n" }))?;
    created(&server, "/triggers", json!({ "name": "lock-log", "tessellation": "order_log", "timing": "before", "function": "refuse" }))?;

    let second = server.insert("orders", &json!({ "total": 10 }))?;
    server.delete("orders", &second)?;
    wait_until(Duration::from_secs(15), "the after trigger's log entries", || {
        let entries = log_entries(&server)?;
        Ok(entries.iter().any(|e| e["order"] == second && e["event"] == "insert") && entries.iter().any(|e| e["order"] == second && e["event"] == "delete"))
    })?;
    assert!(log_entries(&server)?.iter().all(|e| e["order"] != id), "the trigger only sees changes after it was created");
    // Direct writes to the log are still refused.
    assert_eq!(server.request(Method::POST, "/order_log", Some(&json!({ "x": 1 })), &[])?.status.as_u16(), 422);

    // The change feed tells inserts from updates and names the trigger behind a write.
    let feed = server.request(Method::GET, "/changes?after=0&limit=1000", None, &[])?.body;
    let changes = feed["changes"].as_array().unwrap();
    assert!(changes.iter().any(|c| c["id"] == id && c["event"] == "insert"));
    assert!(changes.iter().any(|c| c["id"] == id && c["event"] == "update"));
    assert!(changes.iter().any(|c| c["tessellation"] == "order_log" && c["trigger"] == "log-orders"), "{}", feed);

    // Status, disabling, and deleting.
    let status = server.request(Method::GET, "/triggers/log-orders", None, &[])?.body;
    assert!(status["status"]["runs"].as_u64().unwrap() >= 2, "{}", status);
    let res = server.request(Method::PUT, "/triggers/check-orders", Some(&json!({ "tessellation": "orders", "timing": "before", "function": "check_order", "enabled": false })), &[])?;
    assert_eq!(res.status.as_u16(), 200, "{}", res.body);
    assert_eq!(server.request(Method::POST, "/orders", Some(&json!({ "total": -1 })), &[])?.status.as_u16(), 201, "disabled");
    assert_eq!(server.request(Method::DELETE, "/triggers/check-orders", None, &[])?.status.as_u16(), 204);
    let list = server.request(Method::GET, "/triggers", None, &[])?.body;
    assert_eq!(list["triggers"].as_array().unwrap().len(), 2, "{}", list);
    Ok(())
}
