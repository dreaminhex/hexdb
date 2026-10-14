//! The AI client against a local mock provider: Claude's Messages API and
//! OpenAI-style chat completions, the key handling, usage per feature, the
//! monthly budget, the `ai` permission, and the query advisor.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

const KEY: &str = "sk-test-mock-key-0451";

/// What the mock saw: (path, headers lowercased, body).
type Seen = Arc<Mutex<Vec<(String, Vec<(String, String)>, Value)>>>;

/// A provider that answers every request, recording it. `fail` makes it answer 401.
fn mock_provider(fail: Arc<Mutex<bool>>) -> Result<(u16, Seen)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let seen: Seen = Arc::default();
    let record = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
            let mut headers = Vec::new();
            let mut length = 0;
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap_or(0);
                let h = h.trim_end().to_string();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
                    if k == "content-length" {
                        length = v.parse().unwrap_or(0);
                    }
                    headers.push((k, v));
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).ok();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            record.lock().unwrap().push((path.clone(), headers, body));
            let (status, reply) = if *fail.lock().unwrap() {
                ("401 Unauthorized", json!({ "error": { "message": format!("invalid api key {}", KEY) } }))
            } else if path.ends_with("/v1/messages") {
                let system = record.lock().unwrap().last().map(|c| c.2["system"].as_str().unwrap_or_default().to_string()).unwrap_or_default();
                let answer = if system.contains("thumbnail for a video") {
                    r#"{"frame": 2, "reason": "clear"}"#
                } else if system.contains("tag files") {
                    r#"Here: {"tags": ["Beekeeping", "honey", "beekeeping"], "description": "Notes about bees."}"#
                } else {
                    "OK"
                };
                ("200 OK", json!({ "content": [{ "type": "text", "text": answer }], "stop_reason": "end_turn", "usage": { "input_tokens": 11, "output_tokens": 3 } }))
            } else {
                ("200 OK", json!({ "choices": [{ "message": { "content": "[]" }, "finish_reason": "stop" }], "usage": { "prompt_tokens": 7, "completion_tokens": 2 } }))
            };
            let text = reply.to_string();
            let mut stream = stream;
            let _ = write!(stream, "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", status, text.len(), text);
        }
    });
    Ok((port, seen))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn set(server: &TestServer, changes: Value) -> Result<()> {
    let res = server.request(Method::PUT, "/config", Some(&json!({ "changes": changes })), &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    Ok(())
}

fn usage(server: &TestServer) -> Result<Value> {
    Ok(server.request(Method::GET, "/ai", None, &[])?.body["usage"].clone())
}

#[test]
fn providers_keys_usage_budget_and_permissions() -> Result<()> {
    let server = TestServer::start()?;
    let fail = Arc::new(Mutex::new(false));
    let (port, seen) = mock_provider(fail.clone())?;

    // Nothing configured: nothing is sent.
    let res = server.request(Method::POST, "/ai/test", None, &[])?;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    assert!(seen.lock().unwrap().is_empty());

    // Claude's Messages API, with the lattice's key.
    set(&server, json!({ "ai.provider": "anthropic", "ai.base_url": format!("http://127.0.0.1:{}", port), "ai.api_key": KEY, "ai.model": "claude-test" }))?;
    let res = server.request(Method::POST, "/ai/test", None, &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["ok"], true, "{}", res.body);
    assert_eq!(res.body["reply"], "OK");
    {
        let calls = seen.lock().unwrap();
        let (path, headers, body) = calls.last().unwrap();
        assert_eq!(path, "/v1/messages");
        assert_eq!(header(headers, "x-api-key"), Some(KEY));
        assert_eq!(header(headers, "anthropic-version"), Some("2023-06-01"));
        assert_eq!(header(headers, "anthropic-beta"), None, "no fallbacks away from the official API");
        assert_eq!(body["model"], "claude-test");
        assert!(body["max_tokens"].as_u64().unwrap() > 0);
        assert!(body.get("fallbacks").is_none());
    }
    let u = usage(&server)?;
    assert_eq!((u["input_tokens"].as_u64(), u["output_tokens"].as_u64(), u["calls"].as_u64()), (Some(11), Some(3), Some(1)), "{}", u);
    assert_eq!(u["features"]["test"]["calls"], 1);

    // OpenAI-compatible chat completions (e.g. Ollama).
    set(&server, json!({ "ai.provider": "openai_compatible", "ai.base_url": format!("http://127.0.0.1:{}/v1", port), "ai.model": "llama-test" }))?;
    let res = server.request(Method::POST, "/ai/test", None, &[])?;
    assert_eq!(res.body["ok"], true, "{}", res.body);
    {
        let calls = seen.lock().unwrap();
        let (path, headers, body) = calls.last().unwrap();
        assert_eq!(path, "/v1/chat/completions");
        assert_eq!(header(headers, "authorization").map(String::from), Some(format!("Bearer {}", KEY)));
        assert_eq!(body["model"], "llama-test");
        assert!(body["max_tokens"].as_u64().is_some());
        assert_eq!(body["messages"][0]["role"], "system");
    }
    assert_eq!(usage(&server)?["input_tokens"], 18);

    // OpenAI's own API takes max_completion_tokens; Azure OpenAI authenticates with an api-key header.
    set(&server, json!({ "ai.provider": "openai", "ai.base_url": format!("http://127.0.0.1:{}/v1", port), "ai.model": "gpt-test" }))?;
    assert_eq!(server.request(Method::POST, "/ai/test", None, &[])?.body["ok"], true);
    {
        let calls = seen.lock().unwrap();
        let (_, headers, body) = calls.last().unwrap();
        assert!(body["max_completion_tokens"].as_u64().is_some() && body.get("max_tokens").is_none(), "{}", body);
        assert_eq!(header(headers, "authorization").map(String::from), Some(format!("Bearer {}", KEY)));
    }
    set(&server, json!({ "ai.provider": "azure_openai", "ai.base_url": format!("http://127.0.0.1:{}/openai/v1", port), "ai.model": "deployment-test" }))?;
    assert_eq!(server.request(Method::POST, "/ai/test", None, &[])?.body["ok"], true);
    {
        let calls = seen.lock().unwrap();
        let (path, headers, _) = calls.last().unwrap();
        assert_eq!(path, "/openai/v1/chat/completions");
        assert_eq!(header(headers, "api-key"), Some(KEY));
        assert_eq!(header(headers, "authorization"), None);
    }
    // Without a model, only Claude has a default.
    set(&server, json!({ "ai.provider": "github", "ai.model": null, "ai.base_url": null }))?;
    assert_eq!(server.request(Method::POST, "/ai/test", None, &[])?.status, StatusCode::BAD_REQUEST);
    set(&server, json!({ "ai.provider": "openai_compatible", "ai.base_url": format!("http://127.0.0.1:{}/v1", port), "ai.model": "llama-test" }))?;

    // A provider error comes back without the key.
    *fail.lock().unwrap() = true;
    let res = server.request(Method::POST, "/ai/test", None, &[])?;
    assert_eq!(res.body["ok"], false, "{}", res.body);
    assert!(res.body["error"].as_str().unwrap().contains("401"), "{}", res.body);
    assert!(!res.body.to_string().contains(KEY), "{}", res.body);
    assert_eq!(usage(&server)?["failures"], 1);
    *fail.lock().unwrap() = false;

    // Plugins and scripts need the `ai` permission.
    let reader = server.user_with_roles("rita", json!([{ "name": "reader", "tessellations": ["*"] }]))?;
    let res = server.request_as(Some(&reader), Method::POST, "/ai/complete", Some(&json!({ "prompt": "hi" })), &[])?;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    assert_eq!(server.request_as(Some(&reader), Method::GET, "/ai", None, &[])?.status, StatusCode::FORBIDDEN);
    let bot = server.user_with_roles("bot", json!([{ "name": "ai", "tessellations": [] }]))?;
    let res = server.request_as(Some(&bot), Method::POST, "/ai/complete", Some(&json!({ "prompt": "Keywords for: hexagons", "system": "JSON only" })), &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["text"], "[]");
    {
        let calls = seen.lock().unwrap();
        let body = &calls.last().unwrap().2;
        assert_eq!(body["messages"][0]["content"], "JSON only");
        assert_eq!(body["messages"][1]["content"][0]["text"], "Keywords for: hexagons");
    }
    assert_eq!(usage(&server)?["features"]["api:bot"]["calls"], 1);

    // The query advisor uses the same provider.
    server.insert("notes", &json!({ "status": "live" }))?;
    let res = server.request(Method::GET, "/tessellations/notes/advice?ai=true", None, &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["ai"]["available"], true, "{}", res.body);
    assert_eq!(res.body["ai"]["model"], "llama-test");
    assert_eq!(usage(&server)?["features"]["advisor"]["calls"], 1);

    // The monthly budget stops calls once it's used up.
    let total = usage(&server)?["total_tokens"].as_u64().unwrap();
    set(&server, json!({ "ai.monthly_token_budget": total + 1 }))?;
    assert_eq!(server.request(Method::POST, "/ai/test", None, &[])?.body["ok"], true);
    let res = server.request_as(Some(&bot), Method::POST, "/ai/complete", Some(&json!({ "prompt": "again" })), &[])?;
    assert_eq!(res.status, StatusCode::TOO_MANY_REQUESTS, "{}", res.body);
    let before = seen.lock().unwrap().len();
    let res = server.request(Method::POST, "/ai/test", None, &[])?;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["ok"], false);
    assert_eq!(seen.lock().unwrap().len(), before, "nothing sent over budget");
    let u = usage(&server)?;
    assert_eq!(u["remaining"], 0, "{}", u);
    Ok(())
}

fn wait_for(server: &TestServer, id: &str, what: &str, check: impl Fn(&Value) -> bool) -> Result<Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let doc = server.get_doc("media", id)?.unwrap_or(Value::Null);
        if check(&doc) {
            return Ok(doc);
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("timed out waiting for {}: {}", what, doc);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn upload(server: &TestServer, path: &str, bytes: Vec<u8>) -> Result<String> {
    let res = server.send_bytes(Method::PUT, &format!("/media/_files/{}", path), bytes, &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    Ok(res.body["id"].as_str().unwrap().to_string())
}

fn system_of(call: &(String, Vec<(String, String)>, Value)) -> &str {
    call.2["system"].as_str().unwrap_or_default()
}

#[test]
fn files_are_tagged_and_videos_get_an_ai_thumbnail() -> Result<()> {
    let server = TestServer::start()?;
    let (port, seen) = mock_provider(Arc::new(Mutex::new(false)))?;
    let res = server.request(Method::POST, "/tessellations", Some(&json!({ "name": "media", "kind": "files" })), &[])?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // Uploaded before tagging is on: left alone, and counted in the estimate.
    let early = upload(&server, "notes/early.txt", b"Hive inspection notes for the spring.".to_vec())?;
    wait_for(&server, &early, "extraction", |d| d["extract"]["status"] == "done")?;
    set(&server, json!({
        "ai.provider": "anthropic", "ai.base_url": format!("http://127.0.0.1:{}", port), "ai.api_key": KEY, "ai.model": "claude-test",
        "ai.tagging.enabled": true, "ai.tagging.send": ["metadata", "text", "thumbnail"],
        "ai.video_thumbnails.enabled": true, "ai.video_thumbnails.frames": 3,
    }))?;
    assert!(server.get_doc("media", &early)?.unwrap().get("ai").is_none(), "turning it on doesn't tag existing files");
    let estimate = server.request(Method::GET, "/ai/estimate", None, &[])?.body;
    let media = estimate["tessellations"].as_array().unwrap().iter().find(|t| t["tessellation"] == "media").unwrap().clone();
    assert_eq!(media["tagging"]["untagged"], 1, "{}", estimate);
    assert!(media["tagging"]["tokens_for_untagged"].as_u64().unwrap() > 0);

    // A new file is tagged after extraction, and the tags are searchable.
    let id = upload(&server, "notes/bees.txt", b"Notes on keeping bees: hives, frames, and the spring honey harvest.".to_vec())?;
    let doc = wait_for(&server, &id, "AI tags", |d| d["ai"]["status"] == "done")?;
    assert_eq!(doc["ai"]["tags"], json!(["beekeeping", "honey"]), "{}", doc["ai"]);
    assert_eq!(doc["ai"]["description"], "Notes about bees.");
    assert_eq!(doc["ai"]["sha256"], doc["sha256"]);
    {
        let calls = seen.lock().unwrap();
        let call = calls.iter().rev().find(|c| system_of(c).contains("tag files")).unwrap();
        let parts = call.2["messages"][0]["content"].as_array().unwrap();
        assert!(parts.iter().any(|p| p["text"].as_str().is_some_and(|t| t.contains("bees.txt"))), "metadata sent");
        assert!(parts.iter().any(|p| p["text"].as_str().is_some_and(|t| t.contains("honey harvest"))), "text sent");
    }
    let res = server.request(Method::POST, "/media/_query", Some(&json!({ "filter": { "$text": "beekeeping" } })), &[])?;
    assert!(res.body["documents"].as_array().unwrap().iter().any(|d| d["id"] == id.as_str()), "{}", res.body);
    // Only the AI writes `ai`; user tags stay the user's.
    server.patch("media", &json!({ "id": id, "ai": { "tags": ["forged"] }, "tags": ["mine"] }))?;
    let doc = server.get_doc("media", &id)?.unwrap();
    assert_eq!(doc["ai"]["tags"], json!(["beekeeping", "honey"]));
    assert_eq!(doc["tags"], json!(["mine"]));

    // A video: the AI picks one of three frames.
    let video = hexdb_core::extract::video::fixtures::mp4(&[[220, 30, 30], [30, 200, 30], [30, 30, 220]], 64, 48);
    let vid = upload(&server, "clips/colors.mp4", video)?;
    let doc = wait_for(&server, &vid, "an AI thumbnail", |d| d["thumbnail"]["chosen_by"] == "ai")?;
    assert_eq!((doc["thumbnail"]["frame"].as_u64(), doc["thumbnail"]["of"].as_u64()), (Some(2), Some(3)), "{}", doc["thumbnail"]);
    let picks = || seen.lock().unwrap().iter().filter(|c| system_of(c).contains("thumbnail for a video")).count();
    {
        let calls = seen.lock().unwrap();
        let call = calls.iter().rev().find(|c| system_of(c).contains("thumbnail for a video")).unwrap();
        let images = call.2["messages"][0]["content"].as_array().unwrap().iter().filter(|p| p["type"] == "image").count();
        assert_eq!(images, 3);
    }
    let (status, _, bytes) = server.get_bytes(&format!("/media/{}/thumbnail", vid), &[])?;
    assert_eq!(status, StatusCode::OK);
    let thumb = image::load_from_memory(&bytes)?.to_rgb8();
    let px = *thumb.get_pixel(thumb.width() / 2, thumb.height() / 2);
    assert!(px[1] > px[0] && px[1] > px[2], "the green frame (2 of 3): {:?}", px);
    // Re-reading the metadata keeps the AI's choice without asking again.
    let before = picks();
    let res = server.request(Method::POST, &format!("/media/_reextract?id={}", vid), None, &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert_eq!(server.get_doc("media", &vid)?.unwrap()["thumbnail"]["chosen_by"], "ai");
    assert_eq!(picks(), before);

    // Existing files are tagged when an administrator asks.
    let writer = server.user_with_roles("wren", json!([{ "name": "writer", "tessellations": ["media"] }]))?;
    assert_eq!(server.request_as(Some(&writer), Method::POST, "/media/_ai_tag", None, &[])?.status, StatusCode::FORBIDDEN, "it spends tokens: admins only");
    let res = server.request(Method::POST, "/media/_ai_tag", None, &[])?;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
    assert!(res.body["queued"].as_u64().unwrap() >= 1, "{}", res.body);
    wait_for(&server, &early, "the early file's tags", |d| d["ai"]["status"] == "done")?;

    // A field some role can't see is never sent, so the AI can't repeat it to that role.
    let res = server.request(Method::POST, "/roles", Some(&json!({ "name": "no-text", "permissions": ["read"], "restrictions": { "media": { "hide": ["meta.text"] } } })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    let secret = upload(&server, "notes/plans.txt", b"The launch codes are 0000.".to_vec())?;
    wait_for(&server, &secret, "tags without hidden fields", |d| d["ai"]["status"] == "done")?;
    {
        let calls = seen.lock().unwrap();
        let call = calls.iter().rev().find(|c| system_of(c).contains("tag files")).unwrap();
        let sent = call.2.to_string();
        assert!(sent.contains("plans.txt"), "the file is still described");
        assert!(!sent.contains("launch codes"), "hidden text was sent: {}", sent);
    }

    let u = usage(&server)?;
    assert!(u["features"]["files.tagging"]["calls"].as_u64().unwrap() >= 2, "{}", u);
    assert_eq!(u["features"]["files.video_thumbnails"]["calls"], 1, "{}", u);
    let audit = server.request(Method::GET, "/audit?action=ai.", None, &[])?.body;
    assert!(audit["events"].as_array().unwrap().iter().any(|e| e["action"] == "ai.tag"), "{}", audit);
    Ok(())
}

#[test]
fn tagging_can_send_video_frames_and_whole_pdfs() -> Result<()> {
    let server = TestServer::start()?;
    let (port, seen) = mock_provider(Arc::new(Mutex::new(false)))?;
    let res = server.request(Method::POST, "/tessellations", Some(&json!({ "name": "media", "kind": "files" })), &[])?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    set(&server, json!({
        "ai.provider": "anthropic", "ai.base_url": format!("http://127.0.0.1:{}", port), "ai.api_key": KEY, "ai.model": "claude-test",
        "ai.tagging.enabled": true, "ai.tagging.send": ["metadata", "thumbnail", "video", "pdf"], "ai.tagging.video_frames": 2,
    }))?;
    let tag_call = |name: &str| -> Vec<Value> {
        let calls = seen.lock().unwrap();
        let call = calls.iter().rev().find(|c| system_of(c).contains("tag files") && c.2.to_string().contains(name)).expect("a tagging call");
        call.2["messages"][0]["content"].as_array().unwrap().clone()
    };

    // A video: frames from through it, instead of the one thumbnail.
    let video = hexdb_core::extract::video::fixtures::mp4(&[[220, 30, 30], [30, 200, 30], [30, 30, 220]], 64, 48);
    let vid = upload(&server, "clips/colors.mp4", video)?;
    wait_for(&server, &vid, "the video's tags", |d| d["ai"]["status"] == "done")?;
    let parts = tag_call("colors.mp4");
    assert_eq!(parts.iter().filter(|p| p["type"] == "image").count(), 2, "{:?}", parts);
    assert!(parts.iter().any(|p| p["text"].as_str().is_some_and(|t| t.contains("frames from the video"))));

    // A PDF: sent whole as a document.
    let pdf = b"%PDF-1.4\n1 0 obj <</Type /Catalog>> endobj\ntrailer <</Root 1 0 R>>\n%%EOF\n".to_vec();
    let doc = upload(&server, "docs/report.pdf", pdf)?;
    wait_for(&server, &doc, "the PDF's tags", |d| d["ai"]["status"] == "done")?;
    let parts = tag_call("report.pdf");
    let document = parts.iter().find(|p| p["type"] == "document").expect("the PDF is sent");
    assert_eq!(document["source"]["media_type"], "application/pdf");

    // The estimate counts frames and pages.
    let estimate = server.request(Method::GET, "/ai/estimate", None, &[])?.body;
    let media = estimate["tessellations"].as_array().unwrap().iter().find(|t| t["tessellation"] == "media").unwrap().clone();
    assert!(media["tagging"]["tokens_per_file"].as_u64().unwrap() > 1000, "{}", estimate);
    Ok(())
}
