//! Schemas: validation on write, versioned migrations of existing documents,
//! and compatibility checks.

use anyhow::{bail, Result};
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn wait_for_migration(server: &TestServer, tess: &str, version: u64) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let body = server.request(Method::GET, &format!("/tessellations/{}/schemas", tess), None, &[])?.body;
        let m = &body["migration"];
        if m["version"] == version && (m["state"] == "done" || m["state"] == "failed") {
            return Ok(m.clone());
        }
        if Instant::now() > deadline {
            bail!("migration didn't finish: {}", body);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn schemas_validate_writes_and_migrate_documents() -> Result<()> {
    let server = TestServer::start()?;
    // Documents written before there was a schema.
    let old = server.insert("orders", &json!({ "customer": "ada", "amount": "12.50", "note": "rush" }))?;
    let bad = server.insert("orders", &json!({ "amount": "3" }))?;

    // Version 1 describes the old shape.
    let v1 = json!({ "fields": {
        "customer": { "type": "string", "required": true },
        "amount": { "type": "string", "required": true },
        "note": { "type": "string" },
    }});
    let res = server.request(Method::POST, "/tessellations/orders/schemas", Some(&v1), &[])?;
    assert_eq!(res.status, 201, "{}", res.body);
    assert_eq!(res.body["version"], 1);
    let m = wait_for_migration(&server, "orders", 1)?;
    assert_eq!((m["migrated"].as_u64(), m["failed"].as_u64()), (Some(1), Some(1)), "{}", m);
    assert_eq!(m["errors"][0]["id"], bad.as_str());
    assert_eq!(server.get_doc("orders", &old)?.unwrap()["_schema"], 1);

    // Writes are validated.
    let res = server.request(Method::POST, "/orders", Some(&json!({ "customer": 7, "amount": "1" })), &[])?;
    assert_eq!(res.status, 422, "{}", res.body);
    assert_eq!(res.error_code(), Some("schema_violation"));
    assert!(res.body["error"]["message"].as_str().unwrap().contains("customer must be a string"));

    // An incompatible version is refused; the check endpoint says why.
    let incompatible = json!({ "fields": { "customer": { "type": "string", "required": true }, "total": { "type": "number", "required": true } } });
    let check = server.request(Method::POST, "/tessellations/orders/schemas/check", Some(&incompatible), &[])?;
    assert_eq!(check.body["compatible"], false, "{}", check.body);
    assert!(check.body["error"].as_str().unwrap().contains("'total' is required"));
    assert_eq!(server.request(Method::POST, "/tessellations/orders/schemas", Some(&incompatible), &[])?.status, 409);

    // Version 2: amount -> total as a number, a new status with a default, note dropped.
    let v2 = json!({
        "fields": {
            "customer": { "type": "string", "required": true },
            "total": { "type": "number", "required": true, "min": 0 },
            "status": { "type": "string", "default": "new", "enum": ["new", "paid"] },
        },
        "migration": [
            { "rename": { "from": "amount", "to": "total" } },
            { "convert": { "field": "total", "to": "number" } },
            { "remove": "note" },
        ],
    });
    let check = server.request(Method::POST, "/tessellations/orders/schemas/check", Some(&v2), &[])?;
    assert_eq!((check.body["compatible"].as_bool(), check.body["would_not_fit"].as_u64()), (Some(true), Some(1)), "{}", check.body);
    assert_eq!(server.request(Method::POST, "/tessellations/orders/schemas", Some(&v2), &[])?.status, 201);
    wait_for_migration(&server, "orders", 2)?;
    let migrated = server.get_doc("orders", &old)?.unwrap();
    assert_eq!(migrated, json!({ "id": old, "customer": "ada", "total": 12.5, "status": "new", "_schema": 2 }));

    // New writes use the new shape and get defaults.
    let id = server.insert("orders", &json!({ "customer": "bo", "total": 5 }))?;
    assert_eq!(server.get_doc("orders", &id)?.unwrap()["status"], "new");
    assert_eq!(server.request(Method::POST, "/orders", Some(&json!({ "customer": "bo", "amount": "5" })), &[])?.status, 422);

    // The schema survives a restart, and can be removed.
    let mut server = server;
    server.restart()?;
    let schemas = server.request(Method::GET, "/tessellations/orders/schemas", None, &[])?;
    assert_eq!(schemas.body["current"], 2);
    assert_eq!(server.request(Method::DELETE, "/tessellations/orders/schemas", None, &[])?.status, 204);
    server.insert("orders", &json!({ "anything": true }))?;
    Ok(())
}

fn people(server: &TestServer) -> Result<Vec<Value>> {
    let res = server.request(Method::POST, "/people/_query", Some(&json!({ "limit": 100, "sort": "n" })), &[])?;
    Ok(res.body["documents"].as_array().cloned().unwrap_or_default())
}

#[test]
fn function_steps_migrate_in_batches_and_versions_roll_back() -> Result<()> {
    let server = TestServer::start()?;
    for (n, name) in ["Ada Lovelace", "Grace Hopper", "Alan Turing"].iter().enumerate() {
        server.insert("people", &json!({ "n": n, "name": name }))?;
    }
    let v1 = json!({ "fields": { "name": { "type": "string", "required": true } } });
    assert_eq!(server.request(Method::POST, "/tessellations/people/schemas", Some(&v1), &[])?.status.as_u16(), 201);
    wait_for_migration(&server, "people", 1)?;

    // A migration function and its undo, run over batches of documents.
    for (name, code) in [
        ("split_name", "import json, sys\ndocs = json.load(sys.stdin)['migration']['documents']\nfor d in docs:\n    first, _, last = d.pop('name').partition(' ')\n    d['first'], d['last'] = first, last\nprint(json.dumps({'documents': docs}))\n"),
        ("join_name", "import json, sys\ndocs = json.load(sys.stdin)['migration']['documents']\nfor d in docs:\n    d['name'] = (d.pop('first', '') + ' ' + d.pop('last', '')).strip()\nprint(json.dumps({'documents': docs}))\n"),
    ] {
        let res = server.request(Method::POST, "/functions", Some(&json!({ "name": name, "kind": "script", "runtime": "python", "code": code })), &[])?;
        assert_eq!(res.status.as_u16(), 201, "{}", res.body);
    }
    let v2 = json!({
        "fields": { "first": { "type": "string", "required": true }, "last": { "type": "string", "required": true } },
        "migration": [{ "function": { "name": "split_name", "undo": "join_name" } }]
    });
    // The dry run runs the function on the existing documents.
    let check = server.request(Method::POST, "/tessellations/people/schemas/check", Some(&v2), &[])?.body;
    assert_eq!((check["compatible"].clone(), check["checked"].clone(), check["would_not_fit"].clone()), (json!(true), json!(3), json!(0)), "{}", check);
    // A function that doesn't exist is refused.
    let missing = json!({ "fields": {}, "migration": [{ "function": { "name": "nope" } }] });
    assert_eq!(server.request(Method::POST, "/tessellations/people/schemas", Some(&missing), &[])?.status.as_u16(), 400);

    let res = server.request(Method::POST, "/tessellations/people/schemas", Some(&v2), &[])?;
    assert_eq!(res.status.as_u16(), 201, "{}", res.body);
    assert_eq!(res.body["created_by_login"], "admin");
    let m = wait_for_migration(&server, "people", 2)?;
    assert_eq!((m["state"].clone(), m["migrated"].clone()), (json!("done"), json!(3)), "{}", m);
    let docs = people(&server)?;
    assert_eq!((docs[0]["first"].clone(), docs[0]["last"].clone(), docs[0]["_schema"].clone()), (json!("Ada"), json!("Lovelace"), json!(2)), "{:?}", docs);
    assert!(docs[0].get("name").is_none());

    // Roll back to version 1: version 3 restores its fields, with the inverse migration.
    assert_eq!(server.request(Method::POST, "/tessellations/people/schemas/rollback", Some(&json!({ "to": 2 })), &[])?.status.as_u16(), 409, "already current");
    let res = server.request(Method::POST, "/tessellations/people/schemas/rollback", Some(&json!({ "to": 1 })), &[])?;
    assert_eq!(res.status.as_u16(), 201, "{}", res.body);
    assert_eq!((res.body["version"].clone(), res.body["restores"].clone()), (json!(3), json!(1)));
    assert_eq!(res.body["migration"], json!([{ "function": { "name": "join_name", "undo": "split_name" } }]));
    let m = wait_for_migration(&server, "people", 3)?;
    assert_eq!(m["state"], "done", "{}", m);
    let docs = people(&server)?;
    assert_eq!((docs[1]["name"].clone(), docs[1]["_schema"].clone()), (json!("Grace Hopper"), json!(3)), "{:?}", docs);
    assert!(docs[1].get("first").is_none());
    // Writes follow the restored version.
    assert_eq!(server.request(Method::POST, "/people", Some(&json!({ "first": "x" })), &[])?.status.as_u16(), 422);

    // A removed field can't come back by itself: the rollback says so, and a set_default fills the gap.
    let v4 = json!({ "fields": {}, "migration": [{ "remove": "name" }] });
    assert_eq!(server.request(Method::POST, "/tessellations/people/schemas", Some(&v4), &[])?.status.as_u16(), 201);
    wait_for_migration(&server, "people", 4)?;
    let refused = server.request(Method::POST, "/tessellations/people/schemas/rollback", Some(&json!({ "to": 3 })), &[])?;
    assert_eq!(refused.status.as_u16(), 409, "{}", refused.body);
    assert!(refused.body["error"]["message"].as_str().unwrap().contains("'name' is required"), "{}", refused.body);
    let res = server.request(
        Method::POST,
        "/tessellations/people/schemas/rollback",
        Some(&json!({ "to": 3, "migration": [{ "set_default": { "field": "name", "value": "unknown" } }] })),
        &[],
    )?;
    assert_eq!(res.status.as_u16(), 201, "{}", res.body);
    wait_for_migration(&server, "people", 5)?;
    assert!(people(&server)?.iter().all(|d| d["name"] == "unknown" && d["_schema"] == 5));
    Ok(())
}
