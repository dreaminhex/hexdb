//! Streams: publish/consume with offsets and consumer groups, change sources,
//! webhook destinations, SSE subscriptions and permissions.

use anyhow::{bail, Result};
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn wait_until(timeout: Duration, what: &str, mut check: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !check()? {
        if Instant::now() > deadline {
            bail!("timed out waiting for {}", what);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn webhook() -> Result<(String, Arc<Mutex<Vec<Value>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://{}/in", listener.local_addr()?);
    let got = Arc::new(Mutex::new(Vec::new()));
    let sink = got.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            if reader.read_exact(&mut body).is_ok() {
                if let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(&body) {
                    sink.lock().unwrap().extend(items);
                }
            }
            let _ = reader.into_inner().write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        }
    });
    Ok((url, got))
}

#[test]
fn streams_publish_consume_source_and_deliver() -> Result<()> {
    let server = TestServer::start()?;
    let (url, delivered) = webhook()?;
    let config = json!({
        "name": "big-orders",
        "description": "Orders over 100",
        "retention_hours": 24,
        "sources": [{ "tessellation": "orders", "ops": ["put"], "filter": { "total": { "$gt": 100 } } }],
        "destinations": [{ "url": url, "batch_size": 10 }],
    });
    let res = server.request(Method::POST, "/streams", Some(&config), &[])?;
    assert_eq!(res.status, 201, "{}", res.body);
    assert_eq!(server.request(Method::POST, "/streams", Some(&config), &[])?.status, 409);
    assert_eq!(server.request(Method::POST, "/streams", Some(&json!({ "name": "bad", "destinations": [{ "url": "ftp://x" }] })), &[])?.status, 400);

    // Publish by hand; read back in order.
    let res = server.request(Method::POST, "/streams/big-orders/messages", Some(&json!([{ "payload": { "n": 1 }, "key": "a" }, { "payload": { "n": 2 } }])), &[])?;
    assert_eq!(res.status, 201, "{}", res.body);
    let offsets: Vec<String> = serde_json::from_value(res.body["offsets"].clone())?;
    assert!(offsets[0] < offsets[1]);
    let read = server.request(Method::GET, "/streams/big-orders/messages", None, &[])?;
    assert_eq!(read.body["messages"].as_array().unwrap().len(), 2);
    assert_eq!(read.body["messages"][0]["payload"]["n"], 1);
    assert_eq!(read.body["messages"][0]["key"], "a");
    assert_eq!(read.body["next"], offsets[1].as_str());

    // A consumer group keeps its place.
    let commit = server.request(Method::POST, "/streams/big-orders/groups/billing/commit", Some(&json!({ "offset": offsets[0] })), &[])?;
    assert_eq!(commit.status, 204, "{}", commit.body);
    let rest = server.request(Method::GET, "/streams/big-orders/messages?group=billing", None, &[])?;
    assert_eq!(rest.body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(rest.body["messages"][0]["payload"]["n"], 2);

    // A long poll returns when a message arrives.
    let after = offsets[1].clone();
    let started = Instant::now();
    let base = server.url("");
    let token = server.token().to_string();
    let poller = std::thread::spawn(move || -> Result<Value> {
        let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(20)).build()?;
        Ok(client.get(format!("{}/streams/big-orders/messages?after={}&wait=10", base, after)).bearer_auth(token).send()?.json()?)
    });
    std::thread::sleep(Duration::from_millis(500));
    server.request(Method::POST, "/streams/big-orders/messages", Some(&json!({ "payload": { "n": 3 } })), &[])?;
    let polled = poller.join().unwrap()?;
    assert_eq!(polled["messages"][0]["payload"]["n"], 3);
    assert!(started.elapsed() < Duration::from_secs(8));

    // The source turns matching changes into messages.
    server.insert("orders", &json!({ "total": 50 }))?;
    let big = server.insert("orders", &json!({ "total": 150 }))?;
    wait_until(Duration::from_secs(10), "the source message", || {
        let all = server.request(Method::GET, "/streams/big-orders/messages?limit=100", None, &[])?;
        Ok(all.body["messages"].as_array().unwrap().iter().any(|m| m["payload"]["id"] == big.as_str()))
    })?;
    let all = server.request(Method::GET, "/streams/big-orders/messages?limit=100", None, &[])?;
    let from_source: Vec<&Value> = all.body["messages"].as_array().unwrap().iter().filter(|m| m["headers"]["tessellation"] == "orders").collect();
    assert_eq!(from_source.len(), 1, "only the order over 100");
    assert_eq!(from_source[0]["headers"]["op"], "put");
    assert_eq!(from_source[0]["key"], big.as_str());

    // The destination got every message, in order.
    wait_until(Duration::from_secs(15), "webhook delivery", || Ok(delivered.lock().unwrap().len() >= 4))?;
    let ns: Vec<Value> = delivered.lock().unwrap().iter().take(3).map(|m| m["payload"]["n"].clone()).collect();
    assert_eq!(ns, vec![json!(1), json!(2), json!(3)]);
    let status = server.request(Method::GET, "/streams/big-orders", None, &[])?;
    assert!(status.body["destinations"][0]["delivered"].as_u64().unwrap() >= 4, "{}", status.body);

    // Permissions: a reader of the stream can consume but not publish or configure.
    let reader = server.user_with_roles("rex", json!([{ "name": "reader", "tessellations": ["stream:big-orders"] }]))?;
    assert_eq!(server.request_as(Some(&reader), Method::GET, "/streams/big-orders/messages", None, &[])?.status, 200);
    assert_eq!(server.request_as(Some(&reader), Method::POST, "/streams/big-orders/messages", Some(&json!({ "payload": 1 })), &[])?.status, 403);
    assert_eq!(server.request_as(Some(&reader), Method::DELETE, "/streams/big-orders", None, &[])?.status, 403);
    let listed = server.request_as(Some(&reader), Method::GET, "/streams", None, &[])?;
    assert_eq!(listed.body["streams"].as_array().unwrap().len(), 1);

    // Server-Sent Events from a given offset.
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(10)).build()?;
    let response = client.get(server.url(&format!("/streams/big-orders/subscribe?after={}", offsets[0]))).bearer_auth(server.token()).send()?;
    let mut data = None;
    for line in BufReader::new(response).lines() {
        if let Some(d) = line?.strip_prefix("data:") {
            data = Some(serde_json::from_str::<Value>(d.trim())?);
            break;
        }
    }
    assert_eq!(data.unwrap()["payload"]["n"], 2);

    // Deleting removes it.
    assert_eq!(server.request(Method::DELETE, "/streams/big-orders", None, &[])?.status, 204);
    assert_eq!(server.request(Method::GET, "/streams/big-orders/messages", None, &[])?.status, 404);
    Ok(())
}
