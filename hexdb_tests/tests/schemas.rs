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
