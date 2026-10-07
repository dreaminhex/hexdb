//! The /logs endpoint: startup messages, request logging, filters, and tailing.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::{json, Value};

fn logs(server: &TestServer, query: &str) -> Result<Value> {
    Ok(server.request(Method::GET, &format!("/logs?{}", query), None, &[])?.body)
}

fn messages(body: &Value) -> Vec<String> {
    body["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["message"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn logs_capture_startup_and_writes_and_support_filters() -> Result<()> {
    let server = TestServer::start()?;

    let all = logs(&server, "limit=1000")?;
    let startup = messages(&all);
    assert!(startup.iter().any(|m| m.contains("Logging initialized")), "{:?}", startup);
    assert!(startup.iter().any(|m| m.contains("listening")), "{:?}", startup);
    let seqs: Vec<u64> = all["records"].as_array().unwrap().iter().map(|r| r["seq"].as_u64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "records are oldest first");

    // Tail from the current end: a write shows up as an INFO request record.
    let last = all["last_seq"].as_u64().unwrap();
    server.insert("logtest", &json!({ "name": "a" }))?;
    let tail = logs(&server, &format!("after={}&target=hexdb_api::requests", last))?;
    let tail_messages = messages(&tail);
    assert!(tail_messages.iter().any(|m| m == "POST /logtest -> 201"), "{:?}", tail_messages);
    let record = &tail["records"][0];
    assert_eq!(record["level"], "INFO");
    assert_eq!(record["fields"]["status"], "201");

    // Reads are DEBUG, so the default filter keeps them out of the buffer.
    server.get("/logtest")?;
    let after_read = logs(&server, &format!("after={}&q=GET%20/logtest", last))?;
    assert!(messages(&after_read).is_empty(), "{:?}", messages(&after_read));

    // Client errors are logged; level filtering keeps WARN and above.
    let missing = server.request(Method::DELETE, "/logtest/does-not-exist", None, &[])?;
    assert_eq!(missing.status.as_u16(), 404);
    let found = logs(&server, "q=does-not-exist")?;
    assert_eq!(messages(&found).len(), 1, "{:?}", messages(&found));
    let warnings = logs(&server, "level=warn&limit=1000")?;
    assert!(warnings["records"].as_array().unwrap().iter().all(|r| r["level"] == "WARN" || r["level"] == "ERROR"));

    // Bad level is a 400.
    let bad = server.request(Method::GET, "/logs?level=loud", None, &[])?;
    assert_eq!(bad.status.as_u16(), 400);

    // "logs" is reserved and can't be used as a tessellation name.
    let reserved = server.request(Method::POST, "/tessellations", Some(&json!({ "name": "logs" })), &[])?;
    assert_eq!(reserved.status.as_u16(), 400, "{}", reserved.body);
    Ok(())
}
