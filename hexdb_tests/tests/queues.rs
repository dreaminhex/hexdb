//! Queues over HTTP: permissions per queue, send/receive/ack/nack/extend,
//! long polling, stats, and the 2.0 route names refused for new tessellations.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn post(server: &TestServer, key: Option<&str>, path: &str, body: Value) -> Result<hexdb_tests::ApiResponse> {
    match key {
        Some(key) => server.request_as(Some(key), Method::POST, path, Some(&body), &[]),
        None => server.request(Method::POST, path, Some(&body), &[]),
    }
}

#[test]
fn queues_deliver_to_one_consumer_with_permissions() -> Result<()> {
    let server = TestServer::start()?;
    let res = post(&server, None, "/queues", json!({ "name": "jobs", "visibility_seconds": 2, "max_receives": 3 }))?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(post(&server, None, "/queues", json!({ "name": "jobs" }))?.status, StatusCode::CONFLICT);

    // Producers need write, consumers read, on queue:jobs.
    let producer = server.user_with_roles("pat", json!([{ "name": "writer", "tessellations": ["queue:jobs"] }]))?;
    let consumer = server.user_with_roles("cal", json!([{ "name": "reader", "tessellations": ["queue:jobs"] }]))?;
    let outsider = server.user_with_roles("oz", json!([{ "name": "writer", "tessellations": ["orders"] }]))?;
    assert_eq!(post(&server, Some(&consumer), "/queues/jobs/messages", json!({ "messages": [{ "body": 1 }] }))?.status, StatusCode::FORBIDDEN);
    assert_eq!(post(&server, Some(&outsider), "/queues/jobs/receive", json!({}))?.status, StatusCode::FORBIDDEN);
    let sent = post(&server, Some(&producer), "/queues/jobs/messages", json!({ "messages": [{ "body": { "task": "resize", "id": 7 } }, { "body": "two", "delay_seconds": 600 }] }))?;
    assert_eq!(sent.status, StatusCode::CREATED, "{}", sent.body);
    assert_eq!(sent.body["ids"].as_array().unwrap().len(), 2);

    // One ready message (the other is delayed); it goes to one consumer.
    let got = post(&server, Some(&consumer), "/queues/jobs/receive", json!({ "max": 10 }))?.body;
    let messages = got["messages"].as_array().unwrap().clone();
    assert_eq!(messages.len(), 1, "{}", got);
    assert_eq!(messages[0]["body"], json!({ "task": "resize", "id": 7 }));
    assert!(post(&server, Some(&consumer), "/queues/jobs/receive", json!({ "max": 10 }))?.body["messages"].as_array().unwrap().is_empty(), "leased");
    let receipt = messages[0]["receipt"].as_str().unwrap().to_string();

    // Extend, then nack: it's ready again at once; ack deletes it.
    assert_eq!(post(&server, Some(&consumer), "/queues/jobs/extend", json!({ "receipts": [receipt], "visibility_seconds": 30 }))?.body["extended"], 1);
    assert_eq!(post(&server, Some(&consumer), "/queues/jobs/nack", json!({ "receipts": [receipt] }))?.body["released"], 1);
    let again = post(&server, Some(&consumer), "/queues/jobs/receive", json!({}))?.body;
    let receipt = again["messages"][0]["receipt"].as_str().unwrap().to_string();
    assert_eq!(again["messages"][0]["receives"], 2);
    assert_eq!(post(&server, Some(&consumer), "/queues/jobs/ack", json!({ "receipts": [receipt.clone(), "bogus.receipt"] }))?.body, json!({ "acked": 1, "stale": 1 }));

    // A receive that waits gets a message sent while it waits.
    let started = Instant::now();
    let sender = {
        let url = server.url("/queues/jobs/messages");
        let key = producer.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            reqwest::blocking::Client::new().post(url).bearer_auth(key).json(&json!({ "messages": [{ "body": "late" }] })).send().map(|r| r.status())
        })
    };
    let waited = post(&server, Some(&consumer), "/queues/jobs/receive", json!({ "wait_seconds": 10 }))?.body;
    assert_eq!(sender.join().unwrap()?, StatusCode::CREATED);
    assert_eq!(waited["messages"][0]["body"], "late", "{}", waited);
    assert!(started.elapsed() < Duration::from_secs(8), "woke when the message arrived");

    // Counts: one in flight, one delayed.
    let stats = server.request_as(Some(&consumer), Method::GET, "/queues/jobs", None, &[])?.body;
    assert_eq!((stats["in_flight"].as_u64(), stats["delayed"].as_u64()), (Some(1), Some(1)), "{}", stats);
    // Only those with manage configure or delete it.
    assert_eq!(server.request_as(Some(&producer), Method::DELETE, "/queues/jobs", None, &[])?.status, StatusCode::FORBIDDEN);
    assert_eq!(server.request(Method::DELETE, "/queues/jobs", None, &[])?.status, StatusCode::NO_CONTENT);

    // The names of routes added in 2.0 can't be new tessellations.
    for name in ["queues", "shares", "mcp"] {
        let res = post(&server, None, "/tessellations", json!({ "name": name }))?;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}: {}", name, res.body);
    }
    Ok(())
}
