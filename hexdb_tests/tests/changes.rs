//! The change feed: polling, long polling, Server-Sent Events, and expiry.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};

fn changes(server: &TestServer, query: &str) -> Result<Value> {
    let res = server.request(Method::GET, &format!("/changes?{}", query), None, &[])?;
    assert_eq!(res.status.as_u16(), 200, "{}", res.body);
    Ok(res.body)
}

fn summary(body: &Value) -> Vec<(String, String)> {
    body["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["op"].as_str().unwrap().to_string(), c["tessellation"].as_str().unwrap().to_string()))
        .collect()
}

#[test]
fn polling_returns_committed_changes_in_order() -> Result<()> {
    let server = TestServer::start()?;
    let start = changes(&server, "")?["last_seq"].as_u64().unwrap();

    let id = server.insert("notes", &json!({ "text": "a" }))?;
    server.patch("notes", &json!({ "id": id, "text": "b" }))?;
    let tx = server.request(
        Method::POST,
        "/transactions",
        Some(&json!({ "operations": [
            { "op": "insert", "tessellation": "tasks", "data": { "title": "t" } },
            { "op": "delete", "tessellation": "notes", "id": id },
        ]})),
        &[("Idempotency-Key", "k1")],
    )?;
    assert_eq!(tx.status.as_u16(), 200, "{}", tx.body);
    // System tessellations (users, idempotency records) never appear.
    let user = server.request(Method::POST, "/users", Some(&json!({ "login": "zed", "password": "correct horse battery", "email_address": "zed@example.com", "roles": [] })), &[])?;
    assert_eq!(user.status.as_u16(), 201, "{}", user.body);

    let all = changes(&server, &format!("after={}", start))?;
    assert_eq!(
        summary(&all),
        [("put", "notes"), ("put", "notes"), ("put", "tasks"), ("delete", "notes")]
            .map(|(a, b)| (a.to_string(), b.to_string()))
    );
    let list = all["changes"].as_array().unwrap();
    assert_eq!(list[0]["document"]["text"], "a");
    assert_eq!(list[1]["document"], json!({ "id": id, "text": "b" }));
    assert_eq!(list[3]["id"], id);
    assert!(list[3]["document"].is_null());
    let seqs: Vec<u64> = list.iter().map(|c| c["seq"].as_u64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]));

    // Paging with limit and the cursor.
    let first = changes(&server, &format!("after={}&limit=3", start))?;
    assert_eq!(first["changes"].as_array().unwrap().len(), 3);
    let rest = changes(&server, &format!("after={}", first["last_seq"]))?;
    assert_eq!(summary(&rest), [("delete".to_string(), "notes".to_string())]);

    // Filtering by tessellation.
    let tasks = changes(&server, &format!("after={}&tessellation=tasks", start))?;
    assert_eq!(summary(&tasks), [("put".to_string(), "tasks".to_string())]);

    // Dropping a tessellation is a change too.
    server.delete_tessellation("tasks")?;
    let dropped = changes(&server, &format!("after={}", rest["last_seq"]))?;
    assert!(summary(&dropped).iter().any(|(op, t)| op == "drop_tessellation" && t == "tasks"), "{}", dropped);
    Ok(())
}

#[test]
fn long_polling_wakes_up_on_a_change() -> Result<()> {
    let server = TestServer::start()?;
    server.insert("notes", &json!({ "seed": true }))?;
    let cursor = changes(&server, "")?["last_seq"].as_u64().unwrap();

    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(500));
            server.insert("notes", &json!({ "late": true })).unwrap();
        });
        let started = Instant::now();
        let body = changes(&server, &format!("after={}&wait=20", cursor))?;
        assert!(started.elapsed() < Duration::from_secs(10), "returned as soon as the change arrived");
        assert_eq!(body["changes"][0]["document"]["late"], true);
        Ok(())
    })?;

    // With nothing new, a short wait times out with no changes and the same cursor.
    let last = changes(&server, "")?["last_seq"].as_u64().unwrap();
    let body = changes(&server, &format!("after={}&wait=1", last))?;
    assert!(body["changes"].as_array().unwrap().is_empty());
    assert_eq!(body["last_seq"], last);
    Ok(())
}

#[test]
fn server_sent_events_stream_backlog_and_live_changes() -> Result<()> {
    let server = TestServer::start()?;
    let cursor = changes(&server, "")?["last_seq"].as_u64().unwrap();
    server.insert("notes", &json!({ "n": 1 }))?;
    server.insert("other", &json!({ "n": 0 }))?;

    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let response = client
        .get(server.url(&format!("/changes/stream?after={}&tessellation=notes", cursor)))
        .bearer_auth(server.token())
        .send()?;
    assert_eq!(response.status().as_u16(), 200);
    assert!(response.headers()["content-type"].to_str()?.starts_with("text/event-stream"));

    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(500));
            server.insert("notes", &json!({ "n": 2 })).unwrap();
        });
        // Read events until both notes changes arrive: the backlog one, then the live one.
        let mut seen = Vec::new();
        let mut ids = Vec::new();
        for line in BufReader::new(response).lines() {
            let line = line?;
            if let Some(id) = line.strip_prefix("id:") {
                ids.push(id.trim().parse::<u64>()?);
            }
            if let Some(data) = line.strip_prefix("data:") {
                let change: Value = serde_json::from_str(data.trim())?;
                assert_eq!(change["tessellation"], "notes");
                seen.push(change["document"]["n"].as_i64().unwrap());
                if seen.len() == 2 {
                    break;
                }
            }
        }
        assert_eq!(seen, [1, 2]);
        assert!(ids[0] < ids[1]);
        Ok(())
    })
}

#[test]
fn history_survives_restarts_unless_disabled() -> Result<()> {
    // With the change history (the default), older changes are read from disk.
    let mut server = TestServer::start()?;
    server.insert("notes", &json!({ "n": 1 }))?;
    server.restart()?;
    let res = server.request(Method::GET, "/changes?after=0", None, &[])?;
    assert_eq!(res.status.as_u16(), 200, "{}", res.body);
    assert_eq!(res.body["changes"][0]["document"]["n"], 1);

    // Without it, history starts at startup.
    let mut server = TestServer::start_with(hexdb_tests::TestOptions { storage_toml: "change_history_hours = 0".into(), ..Default::default() })?;
    server.insert("notes", &json!({ "n": 1 }))?;
    server.restart()?;
    let res = server.request(Method::GET, "/changes?after=0", None, &[])?;
    assert_eq!(res.status.as_u16(), 410, "{}", res.body);
    assert_eq!(res.error_code(), Some("history_expired"));
    let res = server.request(Method::GET, "/changes/stream?after=0", None, &[])?;
    assert_eq!(res.status.as_u16(), 410);

    // From the current position everything works.
    let cursor = changes(&server, "")?["last_seq"].as_u64().unwrap();
    server.insert("notes", &json!({ "n": 2 }))?;
    let body = changes(&server, &format!("after={}", cursor))?;
    assert_eq!(body["changes"][0]["document"]["n"], 2);
    Ok(())
}
