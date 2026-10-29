//! Vector search: vector indexes over client vectors and over embeddings the
//! lattice makes itself (against a local mock of an embeddings API), queried
//! by vector or by text through REST, GraphQL and the MCP `similar` tool, as
//! the caller (row filters apply).

use anyhow::{bail, Result};
use hexdb_tests::TestServer;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{atomic::AtomicUsize, atomic::Ordering, Arc};
use std::time::{Duration, Instant};

fn near(server: &TestServer, key: Option<&str>, tess: &str, body: Value) -> Result<hexdb_tests::ApiResponse> {
    match key {
        Some(key) => server.request_as(Some(key), Method::POST, &format!("/{}/_query", tess), Some(&body), &[]),
        None => server.request(Method::POST, &format!("/{}/_query", tess), Some(&body), &[]),
    }
}

fn ids(res: &hexdb_tests::ApiResponse, key: &str) -> Vec<String> {
    res.body["documents"].as_array().into_iter().flatten().filter_map(|d| d[key].as_str().map(String::from)).collect()
}

#[test]
fn client_vectors_are_searched_nearest_first_within_the_callers_reach() -> Result<()> {
    let mut server = TestServer::start()?;
    for (name, vector, region) in [("north", [1.0, 0.0, 0.0], "eu"), ("north-east", [0.7, 0.7, 0.0], "eu"), ("east", [0.0, 1.0, 0.0], "us"), ("up", [0.0, 0.0, 1.0], "us")] {
        server.insert("places", &json!({ "name": name, "region": region, "embedding": vector }))?;
    }
    // No index yet: a clear error.
    let res = near(&server, None, "places", json!({ "near": { "field": "embedding", "vector": [1, 0, 0] } }))?;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    let res = server.request(Method::POST, "/tessellations/places/indexes", Some(&json!({ "kind": "vector", "fields": ["embedding"], "dimensions": 3 })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    assert_eq!(res.body["name"], "embedding_vector");

    let res = near(&server, None, "places", json!({ "near": { "field": "embedding", "vector": [0.9, 0.2, 0.0], "k": 3 } }))?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(ids(&res, "name"), ["north", "north-east", "east"]);
    let scores: Vec<f64> = res.body["documents"].as_array().unwrap().iter().map(|d| d["_score"].as_f64().unwrap()).collect();
    assert!(scores.windows(2).all(|w| w[0] >= w[1]) && scores[0] > 0.97, "{:?}", scores);
    assert_eq!(res.body["plan"]["indexes"], json!(["embedding_vector"]));

    // A filter narrows the candidates; min_score drops weak matches.
    let res = near(&server, None, "places", json!({ "filter": { "region": "us" }, "near": { "field": "embedding", "vector": [1, 0.5, 0], "k": 2 } }))?;
    assert_eq!(ids(&res, "name"), ["east", "up"]);
    let res = near(&server, None, "places", json!({ "near": { "field": "embedding", "vector": [1, 0, 0], "k": 4, "min_score": 0.5 } }))?;
    assert_eq!(ids(&res, "name"), ["north", "north-east"]);

    // Bad requests: wrong dimensions, both or neither of vector and text, k out of range, paging.
    for body in [
        json!({ "near": { "field": "embedding", "vector": [1, 0] } }),
        json!({ "near": { "field": "embedding" } }),
        json!({ "near": { "field": "embedding", "vector": [1, 0, 0], "k": 0 } }),
        json!({ "near": { "field": "embedding", "vector": [1, 0, 0] }, "limit": 5 }),
    ] {
        assert_eq!(near(&server, None, "places", body.clone())?.status, StatusCode::BAD_REQUEST, "{}", body);
    }

    // A user restricted to EU places only ever gets EU places.
    let role = json!({ "name": "eu", "permissions": ["read"], "restrictions": { "places": { "filter": { "region": "eu" } } } });
    assert!(server.request(Method::POST, "/roles", Some(&role), &[])?.status.is_success());
    let key = server.user_with_roles("emma", json!([{ "name": "eu", "tessellations": ["places"] }]))?;
    let res = near(&server, Some(&key), "places", json!({ "near": { "field": "embedding", "vector": [0, 1, 0], "k": 4 } }))?;
    assert_eq!(ids(&res, "name"), ["north-east", "north"]);
    assert_eq!(res.body["plan"]["scanned"], 2, "nothing about other regions leaks");

    // Updates and deletes move the index along; a restart restores it.
    let east = server.request(Method::POST, "/places/_query", Some(&json!({ "filter": { "name": "east" } })), &[])?.body["documents"][0]["id"].as_str().unwrap().to_string();
    server.patch("places", &json!({ "id": east, "embedding": [1, 0, 0.01] }))?;
    let up = server.request(Method::POST, "/places/_query", Some(&json!({ "filter": { "name": "up" } })), &[])?.body["documents"][0]["id"].as_str().unwrap().to_string();
    server.delete("places", &up)?;
    server.restart()?;
    let res = near(&server, None, "places", json!({ "near": { "field": "embedding", "vector": [0, 0, 1], "k": 4 } }))?;
    assert_eq!(res.body["documents"].as_array().map(Vec::len), Some(3), "{}", res.body);
    assert!(!ids(&res, "name").contains(&"up".to_string()));

    // GraphQL.
    let gql = server.request(Method::POST, "/graphql", Some(&json!({ "query": r#"{ similar(tessellation: "places", field: "embedding", vector: [1, 0, 0], k: 1) { documents { json } indexesUsed } }"# })), &[])?;
    assert_eq!(gql.body["data"]["similar"]["indexesUsed"], json!(["embedding_vector"]), "{}", gql.body);
    Ok(())
}

/// An embeddings API (OpenAI's shape) that maps each text to a bag of words
/// over 32 dimensions, so texts sharing words are near each other.
fn mock_embeddings(calls: Arc<AtomicUsize>) -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let mut length = 0;
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap_or(0);
                let h = h.trim_end().to_string();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    if k.trim().eq_ignore_ascii_case("content-length") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).ok();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            calls.fetch_add(1, Ordering::SeqCst);
            let data: Vec<Value> = body["input"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(i, text)| {
                    let mut v = vec![0.0f64; 32];
                    for word in text.as_str().unwrap_or_default().to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
                        let h = word.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
                        v[(h % 32) as usize] += 1.0;
                    }
                    json!({ "index": i, "embedding": v })
                })
                .collect();
            let reply = json!({ "data": data, "model": body["model"], "usage": { "prompt_tokens": 7, "total_tokens": 7 } }).to_string();
            let mut stream = stream;
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", reply.len(), reply);
        }
    });
    Ok(port)
}

fn wait_embedded(server: &TestServer) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status = server.request(Method::GET, "/status", None, &[])?.body;
        if status["embeddings"]["pending"] == 0 && status["embeddings"]["embedded_since_start"].as_u64().unwrap_or(0) > 0 {
            // The last batch's writes may still be landing.
            std::thread::sleep(Duration::from_millis(300));
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("embeddings never finished: {}", status["embeddings"]);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn the_lattice_embeds_source_fields_and_searches_by_text() -> Result<()> {
    let server = TestServer::start()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let port = mock_embeddings(calls.clone())?;
    let config = json!({ "changes": { "ai.embeddings.provider": "openai_compatible", "ai.embeddings.base_url": format!("http://127.0.0.1:{}/v1", port), "ai.embeddings.model": "mock-embed" } });
    assert!(server.request(Method::PUT, "/config", Some(&config), &[])?.status.is_success());

    let mut ids = Vec::new();
    for title in ["red apples and pears", "fast red cars", "quiet mountain lakes", "apples pears plums orchard"] {
        ids.push(server.insert("notes", &json!({ "title": title }))?);
    }
    let res = server.request(Method::POST, "/tessellations/notes/indexes", Some(&json!({ "kind": "vector", "fields": ["embedding"], "source": ["title"] })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    wait_embedded(&server)?;

    let doc = server.get_doc("notes", &ids[0])?.unwrap();
    assert_eq!(doc["embedding"]["model"], "mock-embed", "{}", doc);
    assert_eq!(doc["embedding"]["vector"].as_array().map(Vec::len), Some(32));

    let res = near(&server, None, "notes", json!({ "near": { "field": "embedding", "text": "pears and apples", "k": 2 }, "fields": ["title"] }))?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let titles = ids_of(&res);
    assert!(titles.contains(&"red apples and pears".to_string()) && titles.contains(&"apples pears plums orchard".to_string()), "{:?}", titles);

    // A new title is embedded again; an unrelated change isn't.
    let before = calls.load(Ordering::SeqCst);
    server.patch("notes", &json!({ "id": ids[2], "title": "pears by the lake" }))?;
    wait_until_calls(&calls, before + 1)?;
    wait_embedded(&server)?;
    let res = near(&server, None, "notes", json!({ "near": { "field": "embedding", "text": "lake pears", "k": 1 }, "fields": ["title"] }))?;
    assert_eq!(ids_of(&res), ["pears by the lake"]);
    let settled = calls.load(Ordering::SeqCst);
    server.patch("notes", &json!({ "id": ids[1], "views": 3 }))?;
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(calls.load(Ordering::SeqCst), settled, "the title didn't change, so nothing is embedded");

    // The AI tools (MCP here) search by meaning too.
    let reply = server.request(Method::POST, "/mcp", Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "similar", "arguments": { "tessellation": "notes", "text": "cars" } } })), &[])?;
    let text = reply.body["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
    assert!(text.contains("fast red cars") && !text.contains("\"vector\""), "{}", text);
    Ok(())
}

fn ids_of(res: &hexdb_tests::ApiResponse) -> Vec<String> {
    ids(res, "title")
}

fn wait_until_calls(calls: &AtomicUsize, at_least: usize) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while calls.load(Ordering::SeqCst) < at_least {
        if Instant::now() > deadline {
            bail!("the embeddings API wasn't called");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}
