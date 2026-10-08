//! Runtime settings, document size limits and the disk quota.

use anyhow::Result;
use hexdb_tests::{TestOptions, TestServer};
use reqwest::Method;
use serde_json::{json, Value};

fn setting<'a>(body: &'a Value, key: &str) -> &'a Value {
    body["settings"].as_array().unwrap().iter().find(|s| s["key"] == key).unwrap_or(&Value::Null)
}

#[test]
fn settings_change_live_or_at_restart_and_persist() -> Result<()> {
    let mut server = TestServer::start()?;
    let res = server.request(Method::GET, "/settings", None, &[])?;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(setting(&res.body, "limits.max_document_kb")["value"], 1024);
    assert_eq!(setting(&res.body, "limits.max_document_kb")["live"], true);
    // Secrets are never shown.
    assert_eq!(res.body["config"]["storage"]["encryption_key"], "(set)");
    assert!(!res.body.to_string().contains(hexdb_tests::TEST_ENCRYPTION_KEY));

    // Validation.
    assert_eq!(server.request(Method::PUT, "/settings", Some(&json!({ "limits.max_document_kb": 0 })), &[])?.status, 400);
    assert_eq!(server.request(Method::PUT, "/settings", Some(&json!({ "storage.encryption_key": "x" })), &[])?.status, 400);
    let reader = server.user_with_roles("rita", json!([{ "name": "reader", "tessellations": ["*"] }]))?;
    assert_eq!(server.request_as(Some(&reader), Method::GET, "/settings", None, &[])?.status, 403);

    // A live setting applies at once: a 1 KB document limit.
    let res = server.request(Method::PUT, "/settings", Some(&json!({ "limits.max_document_kb": 1, "memory.ram_mb": 512 })), &[])?;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(setting(&res.body, "limits.max_document_kb")["value"], 1);
    let big = server.request(Method::POST, "/notes", Some(&json!({ "text": "x".repeat(2000) })), &[])?;
    assert_eq!(big.status, 413, "{}", big.body);
    assert_eq!(big.error_code(), Some("document_too_large"));
    server.insert("notes", &json!({ "text": "small" }))?;
    // A restart-only setting waits for the restart.
    assert_eq!(setting(&res.body, "memory.ram_mb")["restart_required"], true);
    assert_eq!(res.body["restart_required"], true);

    // Both persist across a restart, and the restart applies the second.
    server.restart()?;
    let res = server.request(Method::GET, "/settings", None, &[])?;
    assert_eq!(setting(&res.body, "memory.ram_mb")["value"], 512);
    assert_eq!(res.body["restart_required"], false);
    assert_eq!(server.request(Method::POST, "/notes", Some(&json!({ "text": "x".repeat(2000) })), &[])?.status, 413);

    // Removing an override returns to the config file's value.
    let res = server.request(Method::PUT, "/settings", Some(&json!({ "limits.max_document_kb": null })), &[])?;
    assert_eq!(setting(&res.body, "limits.max_document_kb")["value"], 1024, "{}", res.body);
    server.insert("notes", &json!({ "text": "x".repeat(2000) }))?;

    // Changes are audited.
    let audit = server.request(Method::GET, "/audit?action=settings.update", None, &[])?;
    assert_eq!(audit.body["total"], 2, "{}", audit.body);
    Ok(())
}

#[test]
fn writes_stop_at_the_disk_quota_but_deletes_continue() -> Result<()> {
    let server = TestServer::start_with(TestOptions { disk_mb: Some(1), ..Default::default() })?;
    // Fill past 1 MB with data that doesn't compress, then flush so the usage is measured.
    let noise = || (0..400).map(|_| ulid::Ulid::new().to_string()).collect::<String>();
    let chunk: Vec<Value> = (0..40).map(|i| json!({ "i": i, "pad": noise() })).collect();
    let mut first = None;
    for _ in 0..8 {
        let res = server.request(Method::POST, "/notes/_bulk", Some(&Value::Array(chunk.clone())), &[])?;
        if res.status != 201 && res.status != 200 {
            break;
        }
        first = first.or_else(|| res.body["ids"].get(0).and_then(Value::as_str).map(String::from));
    }
    server.flush()?;
    let res = server.request(Method::POST, "/notes", Some(&json!({ "more": true })), &[])?;
    assert_eq!(res.status.as_u16(), 507, "{}", res.body);
    assert_eq!(res.error_code(), Some("disk_full"));
    // Deleting is still allowed.
    if let Some(id) = first {
        server.delete("notes", &id)?;
    }
    Ok(())
}
