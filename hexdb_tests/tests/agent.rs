//! The lattice tools: the MCP server for external agents, and the AI chat
//! (against a local mock of the Claude API). Both read only what the asking
//! user may read, and the chat stays within its limits.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// Orders in two regions, each with a `secret` field one role can't see.
fn seed(server: &TestServer) -> Result<String> {
    for (i, region) in ["eu", "eu", "eu", "us", "us"].iter().enumerate() {
        server.insert("orders", &json!({ "n": i, "region": region, "total": 10 * (i + 1), "secret": format!("card-{}", i) }))?;
    }
    let role = json!({
        "name": "eu_reader",
        "permissions": ["read", "chat"],
        "restrictions": { "orders": { "filter": { "region": "eu" }, "hide": ["secret"] } }
    });
    let res = server.request(Method::POST, "/roles", Some(&role), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    server.user_with_roles("emma", json!([{ "name": "eu_reader", "tessellations": ["orders"] }]))
}

fn mcp(server: &TestServer, key: &str, id: u64, method: &str, params: Value) -> Result<Value> {
    let res = server.request_as(Some(key), Method::POST, "/mcp", Some(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })), &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    Ok(res.body)
}

fn tool(server: &TestServer, key: &str, name: &str, arguments: Value) -> Result<(bool, String)> {
    let reply = mcp(server, key, 9, "tools/call", json!({ "name": name, "arguments": arguments }))?;
    let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
    Ok((reply["result"]["isError"] == true, text))
}

#[test]
fn mcp_reads_only_what_the_caller_may_read() -> Result<()> {
    let server = TestServer::start()?;
    let key = seed(&server)?;
    server.insert("payroll", &json!({ "salary": 100 }))?;

    let init = mcp(&server, &key, 1, "initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } }))?;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "HexDB");
    let res = server.request_as(Some(&key), Method::POST, "/mcp", Some(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })), &[])?;
    assert_eq!(res.status, StatusCode::ACCEPTED);
    let tools = mcp(&server, &key, 2, "tools/list", json!({}))?;
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"query") && names.contains(&"sql"), "{:?}", names);
    assert!(tools["result"]["tools"].as_array().unwrap().iter().all(|t| t["annotations"]["readOnlyHint"] == true));

    // Only the tessellations the key can read, counted through its row filter.
    let (failed, text) = tool(&server, &key, "list_tessellations", json!({}))?;
    assert!(!failed, "{}", text);
    let listed: Value = serde_json::from_str(&text)?;
    assert_eq!(listed["tessellations"], json!([{ "name": "orders", "kind": "documents", "documents": 3 }]), "{}", text);

    // Every tool sees only EU orders, without the hidden field.
    let (_, text) = tool(&server, &key, "query", json!({ "tessellation": "orders", "sort": "total desc" }))?;
    let found: Value = serde_json::from_str(&text)?;
    let docs = found["documents"].as_array().unwrap();
    assert_eq!(docs.len(), 3, "{}", text);
    assert!(docs.iter().all(|d| d["region"] == "eu" && d.get("secret").is_none()), "{}", text);
    let (_, text) = tool(&server, &key, "count", json!({ "tessellation": "orders", "filter": { "region": "us" } }))?;
    assert_eq!(serde_json::from_str::<Value>(&text)?["count"], 0, "US orders are outside the row filter");
    let (_, text) = tool(&server, &key, "aggregate", json!({ "tessellation": "orders", "group_by": ["region"], "aggregates": { "sum": { "$sum": "total" } } }))?;
    assert!(text.contains("\"eu\"") && !text.contains("\"us\""), "{}", text);
    let (_, text) = tool(&server, &key, "sql", json!({ "query": "SELECT region, total FROM orders" }))?;
    assert!(!text.contains("\"us\""), "{}", text);
    let (failed, text) = tool(&server, &key, "sql", json!({ "query": "SELECT secret FROM orders" }))?;
    assert!(failed || !text.contains("card-"), "a hidden field never comes back: {}", text);
    let (failed, text) = tool(&server, &key, "query", json!({ "tessellation": "orders", "filter": { "secret": "card-0" } }))?;
    assert!(failed, "filtering on a hidden field is refused: {}", text);

    // Tessellations without a grant, system tessellations and unknown tools fail.
    let (failed, _) = tool(&server, &key, "query", json!({ "tessellation": "payroll" }))?;
    assert!(failed, "no grant on payroll");
    let (failed, _) = tool(&server, &key, "query", json!({ "tessellation": "users" }))?;
    assert!(failed, "system tessellations aren't readable");
    assert_eq!(mcp(&server, &key, 3, "tools/call", json!({ "name": "delete_everything", "arguments": {} }))?["error"]["code"], -32602);
    assert_eq!(mcp(&server, &key, 4, "resources/list", json!({}))?["error"]["code"], -32601);

    // Credentials are required; there is no event stream.
    let res = server.request_as(None, Method::POST, "/mcp", Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })), &[])?;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
    assert_eq!(server.request_as(Some(&key), Method::GET, "/mcp", None, &[])?.status, StatusCode::METHOD_NOT_ALLOWED);
    Ok(())
}

type Seen = Arc<Mutex<Vec<Value>>>;

/// A Claude API that first asks for a `count` of orders, then answers with what the tool said.
fn mock_claude() -> Result<(u16, Seen)> {
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
            record.lock().unwrap().push(body.clone());
            let last = body["messages"].as_array().and_then(|m| m.last()).cloned().unwrap_or(Value::Null);
            let results: Vec<String> = last["content"].as_array().into_iter().flatten().filter(|c| c["type"] == "tool_result").filter_map(|c| c["content"].as_str().map(String::from)).collect();
            let reviewing = body["system"].as_str().is_some_and(|s| s.contains("security reviewer"));
            let reply = if reviewing {
                let verdict = json!({ "severity": "medium", "summary": "Repeated failed sign-ins from one address.", "findings": [{ "title": "Password guessing", "severity": "medium", "evidence": "failed sign-ins from 127.0.0.1", "recommendation": "Block the address." }] });
                json!({ "content": [{ "type": "text", "text": verdict.to_string() }], "stop_reason": "end_turn", "usage": { "input_tokens": 300, "output_tokens": 60 } })
            } else if let Some(result) = results.first() {
                json!({ "content": [{ "type": "text", "text": format!("The tool said: {}", result) }], "stop_reason": "end_turn", "usage": { "input_tokens": 40, "output_tokens": 10 } })
            } else if body["tools"].is_array() {
                json!({ "content": [{ "type": "text", "text": "Let me count." }, { "type": "tool_use", "id": "call_1", "name": "count", "input": { "tessellation": "orders" } }], "stop_reason": "tool_use", "usage": { "input_tokens": 30, "output_tokens": 8 } })
            } else {
                json!({ "content": [{ "type": "text", "text": "No tools left." }], "stop_reason": "end_turn", "usage": { "input_tokens": 5, "output_tokens": 3 } })
            };
            let (text, kind) = if body["stream"] == true { (as_events(&reply), "text/event-stream") } else { (reply.to_string(), "application/json") };
            let mut stream = stream;
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", kind, text.len(), text);
        }
    });
    Ok((port, seen))
}

/// A Messages API reply as the server-sent events of a streamed one, each
/// text and tool input split in two pieces.
fn as_events(reply: &Value) -> String {
    let mut out = String::new();
    let mut event = |value: Value| out.push_str(&format!("event: {}\ndata: {}\n\n", value["type"].as_str().unwrap(), value));
    event(json!({ "type": "message_start", "message": { "usage": { "input_tokens": reply["usage"]["input_tokens"], "output_tokens": 1 } } }));
    for (index, block) in reply["content"].as_array().unwrap().iter().enumerate() {
        let (start, piece, kind) = if block["type"] == "text" {
            (json!({ "type": "text", "text": "" }), block["text"].as_str().unwrap().to_string(), "text_delta")
        } else {
            (json!({ "type": "tool_use", "id": block["id"], "name": block["name"], "input": {} }), block["input"].to_string(), "input_json_delta")
        };
        event(json!({ "type": "content_block_start", "index": index, "content_block": start }));
        let half = piece.char_indices().nth(piece.chars().count() / 2).map_or(0, |(i, _)| i);
        for part in [&piece[..half], &piece[half..]] {
            let delta = if kind == "text_delta" { json!({ "type": kind, "text": part }) } else { json!({ "type": kind, "partial_json": part }) };
            event(json!({ "type": "content_block_delta", "index": index, "delta": delta }));
        }
        event(json!({ "type": "content_block_stop", "index": index }));
    }
    event(json!({ "type": "message_delta", "delta": { "stop_reason": reply["stop_reason"] }, "usage": { "output_tokens": reply["usage"]["output_tokens"] } }));
    event(json!({ "type": "message_stop" }));
    out
}

/// The events of a streamed answer, as (event, data).
fn streamed(server: &TestServer, key: &str, question: &str) -> Result<Vec<(String, Value)>> {
    let response = hexdb_tests::http().build()?.post(server.url("/ai/chat")).bearer_auth(key).json(&json!({ "question": question, "stream": true })).send()?;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()["content-type"].to_str()?.starts_with("text/event-stream"));
    let text = response.text()?;
    Ok(text
        .split("\n\n")
        .filter_map(|block| {
            let event = block.lines().find_map(|l| l.strip_prefix("event:"))?.trim().to_string();
            let data = block.lines().find_map(|l| l.strip_prefix("data:"))?.trim();
            Some((event, serde_json::from_str(data).ok()?))
        })
        .collect())
}

/// A bare SMTP server that takes messages and hands each one's text over.
fn mock_smtp() -> Result<(u16, std::sync::mpsc::Receiver<String>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let _ = writer.write_all(b"220 mock ESMTP\r\n");
            let mut message = String::new();
            let mut in_data = false;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if in_data {
                    if line == ".\r\n" {
                        in_data = false;
                        let _ = sender.send(std::mem::take(&mut message));
                        let _ = writer.write_all(b"250 queued\r\n");
                    } else {
                        message.push_str(&line);
                    }
                    continue;
                }
                let reply: &[u8] = match line.get(..4).map(str::to_ascii_uppercase).as_deref() {
                    Some("EHLO") | Some("HELO") => b"250 mock\r\n",
                    Some("DATA") => {
                        in_data = true;
                        b"354 go ahead\r\n"
                    }
                    Some("QUIT") => {
                        let _ = writer.write_all(b"221 bye\r\n");
                        break;
                    }
                    _ => b"250 ok\r\n",
                };
                let _ = writer.write_all(reply);
            }
        }
    });
    Ok((port, receiver))
}

#[test]
fn the_ai_reviews_the_audit_trail_and_emails_what_it_finds() -> Result<()> {
    let server = TestServer::start()?;
    let (port, seen) = mock_claude()?;
    let (smtp, inbox) = mock_smtp()?;
    configure(&server, json!({ "ai.provider": "anthropic", "ai.base_url": format!("http://127.0.0.1:{}", port), "ai.api_key": "sk-test-review-01", "ai.model": "claude-test" }))?;
    let reader = server.user_with_roles("rita", json!([{ "name": "reader", "tessellations": ["orders"] }]))?;
    // Something worth a look: failed sign-ins.
    for _ in 0..6 {
        server.request_as(None, Method::POST, "/auth/login", Some(&json!({ "login": "hexdbadmin", "password": "guess" })), &[])?;
    }

    // Administrators only; without email set up the review is kept and says why it wasn't sent.
    assert_eq!(server.request_as(Some(&reader), Method::POST, "/security/reviews", None, &[])?.status, StatusCode::FORBIDDEN);
    let review = server.request(Method::POST, "/security/reviews", None, &[])?;
    assert_eq!(review.status, StatusCode::OK, "{}", review.body);
    assert_eq!(review.body["severity"], "medium", "{}", review.body);
    assert_eq!(review.body["findings"][0]["title"], "Password guessing");
    assert!(review.body["events"].as_u64().unwrap() >= 6, "{}", review.body);
    assert!(review.body["email_error"].as_str().unwrap().contains("email"), "{}", review.body);
    // The model got the counts, not documents.
    let sent = seen.lock().unwrap().iter().rev().find(|b| b["system"].as_str().is_some_and(|s| s.contains("security reviewer"))).cloned().unwrap();
    let summary: Value = serde_json::from_str(sent["messages"][0]["content"][0]["text"].as_str().unwrap())?;
    assert!(summary["failed_sign_ins_by_login"].as_array().unwrap().iter().any(|f| f["key"] == "hexdbadmin" && f["failures"].as_u64().unwrap() >= 6), "{}", summary);

    // With SMTP set up, a test email and the review's email arrive.
    configure(&server, json!({ "email.smtp_url": format!("smtp://127.0.0.1:{}", smtp), "email.from": "HexDB <hexdb@example.com>", "security.review.recipients": "sec@example.com, ops@example.com" }))?;
    let test = server.request(Method::POST, "/security/email-test", Some(&json!({ "to": "me@example.com" })), &[])?;
    assert_eq!(test.status, StatusCode::OK, "{}", test.body);
    let message = inbox.recv_timeout(std::time::Duration::from_secs(10))?;
    assert!(message.contains("To: me@example.com") && message.contains("Outbound email works"), "{}", message);
    let review = server.request(Method::POST, "/security/reviews", None, &[])?;
    assert_eq!(review.body["emailed_to"], json!(["sec@example.com", "ops@example.com"]), "{}", review.body);
    let message = inbox.recv_timeout(std::time::Duration::from_secs(10))?;
    assert!(message.contains("Subject: HexDB security review: medium severity, 1 finding") && message.contains("Password guessing"), "{}", message);

    // Both are listed, newest first, and audited.
    let listed = server.request(Method::GET, "/security/reviews", None, &[])?.body;
    assert_eq!(listed["reviews"].as_array().unwrap().len(), 2, "{}", listed);
    assert_eq!(listed["email"], true);
    assert!(listed["reviews"][0]["started"].as_i64() >= listed["reviews"][1]["started"].as_i64());
    let audit = server.request(Method::GET, "/audit?action=security.review", None, &[])?.body;
    assert_eq!(audit["events"].as_array().unwrap().len(), 2, "{}", audit);
    Ok(())
}

/// A receiver keeping each request's path, headers (lowercase names) and raw body.
type RawReceived = Arc<Mutex<Vec<(String, std::collections::HashMap<String, String>, String)>>>;

fn raw_receiver() -> Result<(u16, RawReceived)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let received: RawReceived = Arc::default();
    let record = received.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
            let mut headers = std::collections::HashMap::new();
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap_or(0);
                let h = h.trim_end().to_string();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                }
            }
            let length = headers.get("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).ok();
            record.lock().unwrap().push((path, headers, String::from_utf8_lossy(&body).into_owned()));
            let mut stream = stream;
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        }
    });
    Ok((port, received))
}

#[test]
fn flagged_reviews_go_to_the_alert_channels() -> Result<()> {
    let server = TestServer::start()?;
    let (ai, _seen) = mock_claude()?;
    let (port, received) = raw_receiver()?;
    configure(&server, json!({ "ai.provider": "anthropic", "ai.base_url": format!("http://127.0.0.1:{}", ai), "ai.api_key": "sk-test-review-02", "ai.model": "claude-test" }))?;
    assert_eq!(server.request(Method::POST, "/security/alert-test", None, &[])?.status, StatusCode::BAD_REQUEST, "nothing set up yet");
    configure(
        &server,
        json!({
            "alerts.webhook.url": format!("http://127.0.0.1:{}/hook", port),
            "alerts.webhook.signing_secret": "whsec-test",
            "alerts.chat.webhook_urls": format!("http://127.0.0.1:{}/chat", port),
        }),
    )?;
    let listed = server.request(Method::GET, "/security/reviews", None, &[])?.body;
    assert_eq!(listed["alert_channels"], json!(["chat", "webhook"]), "{}", listed);
    assert_eq!(server.request(Method::POST, "/security/alert-test", Some(&json!({ "channel": "pigeon" })), &[])?.status, StatusCode::BAD_REQUEST);

    // A test alert reaches both.
    let test = server.request(Method::POST, "/security/alert-test", None, &[])?;
    assert_eq!(test.status, StatusCode::OK, "{}", test.body);
    assert_eq!(test.body["deliveries"], json!([{ "channel": "chat", "ok": true }, { "channel": "webhook", "ok": true }]), "{}", test.body);
    let got = received.lock().unwrap().clone();
    assert_eq!(got.len(), 2);
    let chat: Value = serde_json::from_str(&got[0].2)?;
    assert_eq!(got[0].0, "/chat");
    assert!(chat["text"].as_str().unwrap().contains("test alert"), "Slack's format for an unknown host: {}", chat);
    let (path, headers, body) = &got[1];
    assert_eq!(path, "/hook");
    assert_eq!(headers["x-hexdb-event"], "alert.test");
    let timestamp: i64 = headers["x-hexdb-timestamp"].parse()?;
    assert_eq!(headers["x-hexdb-signature"], format!("sha256={}", hexdb_core::alerts::sign("whsec-test", timestamp, body)), "signed over the bytes sent");

    // A medium review goes to both, and says so.
    let review = server.request(Method::POST, "/security/reviews", None, &[])?;
    assert_eq!(review.body["severity"], "medium", "{}", review.body);
    assert_eq!(review.body["alerts"], json!([{ "channel": "chat", "ok": true }, { "channel": "webhook", "ok": true }]), "{}", review.body);
    assert!(review.body.get("email_error").is_none(), "no email warning when alerts went out: {}", review.body);
    let got = received.lock().unwrap().clone();
    assert_eq!(got.len(), 4);
    assert!(serde_json::from_str::<Value>(&got[2].2)?["text"].as_str().unwrap().contains("Password guessing"));
    let hook: Value = serde_json::from_str(&got[3].2)?;
    assert_eq!((hook["event"].as_str(), hook["review"]["severity"].as_str()), (Some("security.review"), Some("medium")), "{}", hook);
    assert_eq!(hook["review"]["findings"][0]["title"], "Password guessing");

    // Above the threshold only.
    configure(&server, json!({ "security.review.notify_severity": "high" }))?;
    let review = server.request(Method::POST, "/security/reviews", None, &[])?;
    assert_eq!(review.body["alerts"], json!([]), "{}", review.body);
    assert_eq!(received.lock().unwrap().len(), 4);

    // A channel set up wrong fails on its own, without sending anything or showing its credentials.
    configure(&server, json!({ "alerts.twilio.account_sid": "not-a-sid", "alerts.twilio.auth_token": "twilio-token-secret", "alerts.twilio.to": "+15551230001" }))?;
    let sms = server.request(Method::POST, "/security/alert-test", Some(&json!({ "channel": "sms" })), &[])?;
    assert_eq!(sms.body["deliveries"][0]["ok"], false, "{}", sms.body);
    assert!(sms.body["deliveries"][0]["error"].as_str().unwrap().contains("account_sid") && !sms.body.to_string().contains("twilio-token-secret"), "{}", sms.body);
    let audit = server.request(Method::GET, "/audit?action=alert.test", None, &[])?.body;
    assert_eq!(audit["events"].as_array().unwrap().len(), 2, "{}", audit);
    Ok(())
}

fn configure(server: &TestServer, changes: Value) -> Result<()> {
    let res = server.request(Method::PUT, "/config", Some(&json!({ "changes": changes })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    Ok(())
}

fn ask(server: &TestServer, key: &str, question: &str) -> Result<hexdb_tests::ApiResponse> {
    server.request_as(Some(key), Method::POST, "/ai/chat", Some(&json!({ "question": question })), &[])
}

#[test]
fn the_chat_answers_from_the_users_data_within_its_limits() -> Result<()> {
    let server = TestServer::start()?;
    let key = seed(&server)?;
    let (port, seen) = mock_claude()?;
    configure(&server, json!({ "ai.provider": "anthropic", "ai.base_url": format!("http://127.0.0.1:{}", port), "ai.api_key": "sk-test-chat-0001", "ai.model": "claude-test" }))?;

    // Off until an administrator turns it on.
    assert_eq!(ask(&server, &key, "How many orders?")?.status, StatusCode::FORBIDDEN);
    configure(&server, json!({ "ai.chat.enabled": true, "ai.chat.model": "claude-small", "ai.chat.client_questions_per_minute": 3 }))?;

    // The model's lookup runs as the user: three EU orders, not five.
    let res = ask(&server, &key, "How many orders?")?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body["answer"].as_str().unwrap().contains("\"count\":3"), "{}", res.body);
    assert_eq!(res.body["steps"], json!([{ "tool": "count", "input": { "tessellation": "orders" }, "ok": true }]));
    assert_eq!(res.body["model"], "claude-small", "ai.chat.model is used");
    assert_eq!(res.body["tokens"], json!({ "input": 70, "output": 18 }));
    {
        let calls = seen.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[0]["system"].as_str().unwrap().contains("nothing else"), "the chat stays on the lattice's data");
        assert!(calls[0]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "query"));
    }

    // A user without the chat permission, and over-long questions, are refused.
    let reader = server.user_with_roles("rita", json!([{ "name": "reader", "tessellations": ["orders"] }]))?;
    assert_eq!(ask(&server, &reader, "How many orders?")?.status, StatusCode::FORBIDDEN);
    assert_eq!(ask(&server, &key, &"x".repeat(5000))?.status, StatusCode::BAD_REQUEST);

    // Questions per minute from one address are limited (3 here; one asked above).
    assert_eq!(ask(&server, &key, "Again?")?.status, StatusCode::OK);
    assert_eq!(ask(&server, &key, "And again?")?.status, StatusCode::OK);
    let limited = ask(&server, &key, "Once more?")?;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS, "{}", limited.body);
    assert!(limited.headers.contains_key("retry-after"));

    // A question whose next step wouldn't fit its allowance isn't sent at all:
    // each step is estimated (the conversation, the tools, the most the answer may use) before it goes.
    configure(&server, json!({ "ai.chat.client_questions_per_minute": 0, "ai.chat.max_tokens_per_question": 1000 }))?;
    let sent = seen.lock().unwrap().len();
    let too_big = ask(&server, &key, "How many orders?")?;
    assert_eq!(too_big.status, StatusCode::TOO_MANY_REQUESTS, "{}", too_big.body);
    assert_eq!(too_big.error_code(), Some("chat_limit"));
    assert_eq!(seen.lock().unwrap().len(), sent, "nothing reached the provider");
    configure(&server, json!({ "ai.chat.max_tokens_per_question": 20000 }))?;

    // Streamed: the model's text as it's written, the lookup between (the text
    // before it wasn't the answer), then the whole answer, counted like any other.
    let events = streamed(&server, &key, "How many orders, streamed?")?;
    let kinds: Vec<&str> = events.iter().map(|(e, _)| e.as_str()).collect();
    let lookup = kinds.iter().position(|k| *k == "lookup").expect("a lookup event");
    assert!(kinds[..lookup].iter().all(|k| *k == "text") && lookup >= 2, "{:?}", kinds);
    let before: String = events[..lookup].iter().filter_map(|(_, d)| d["text"].as_str()).collect();
    assert_eq!(before, "Let me count.");
    assert_eq!(events[lookup].1, json!({ "tool": "count", "tessellation": "orders" }));
    let after: String = events[lookup + 1..].iter().filter(|(e, _)| e == "text").filter_map(|(_, d)| d["text"].as_str()).collect();
    let (last, answer) = events.last().unwrap();
    assert_eq!(last, "answer", "{:?}", kinds);
    assert_eq!(answer["answer"].as_str(), Some(after.as_str()), "the streamed text is the answer");
    assert!(after.contains("\"count\":3"), "{}", after);
    assert_eq!(answer["tokens"], json!({ "input": 70, "output": 18 }));
    assert!(seen.lock().unwrap().iter().rev().take(2).all(|call| call["stream"] == true));

    // Usage is counted as the chat feature, and each question is audited.
    let usage = server.request(Method::GET, "/ai", None, &[])?.body;
    assert!(usage["usage"]["features"]["chat"]["calls"].as_u64().unwrap() >= 6, "{}", usage);
    let audit = server.request(Method::GET, "/audit?action=ai.chat", None, &[])?.body;
    assert_eq!(audit["events"].as_array().unwrap().len(), 4, "{}", audit);
    Ok(())
}

fn a2a(server: &TestServer, key: &str, method: &str, params: Value, version: &str) -> Result<Value> {
    let res = server.request_as(Some(key), Method::POST, "/a2a", Some(&json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params })), &[("a2a-version", version)])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    Ok(res.body)
}

#[test]
fn a2a_agents_ask_as_the_calling_user() -> Result<()> {
    let server = TestServer::start()?;
    let key = seed(&server)?;
    let (port, _seen) = mock_claude()?;
    configure(&server, json!({ "ai.provider": "anthropic", "ai.base_url": format!("http://127.0.0.1:{}", port), "ai.api_key": "sk-test-a2a-0001", "ai.chat.enabled": true }))?;
    let anon = hexdb_tests::http().build()?;

    // Off by default: no card, no endpoint.
    assert_eq!(anon.get(server.url("/.well-known/agent-card.json")).send()?.status(), StatusCode::NOT_FOUND);
    let send = json!({ "message": { "messageId": "m1", "role": "ROLE_USER", "parts": [{ "text": "How many orders?" }] } });
    assert_eq!(server.request_as(Some(&key), Method::POST, "/a2a", Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": send })), &[])?.status, StatusCode::NOT_FOUND);
    configure(&server, json!({ "a2a.enabled": true }))?;

    // The card is public and points at /a2a; the endpoint needs credentials.
    let card: Value = anon.get(server.url("/.well-known/agent-card.json")).send()?.json()?;
    assert_eq!(card["supportedInterfaces"][0]["protocolBinding"], "JSONRPC");
    assert!(card["supportedInterfaces"][0]["url"].as_str().unwrap().ends_with("/a2a"), "{}", card);
    assert_eq!(card["securitySchemes"]["bearer"]["httpAuthSecurityScheme"]["scheme"], "Bearer");
    assert_eq!(anon.post(server.url("/a2a")).json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": send })).send()?.status(), StatusCode::UNAUTHORIZED);

    // A question runs as the caller (three EU orders, not five) and completes a task.
    let answer = a2a(&server, &key, "SendMessage", send.clone(), "1.0")?;
    let task = &answer["result"]["task"];
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{}", answer);
    assert!(task["artifacts"][0]["parts"][0]["text"].as_str().unwrap().contains("\"count\":3"), "{}", answer);
    assert_eq!(task["history"][0]["role"], "ROLE_USER");
    let (task_id, context) = (task["id"].as_str().unwrap().to_string(), task["contextId"].as_str().unwrap().to_string());

    // Tasks can be read back (by their owner only), listed, and not canceled once done.
    let got = a2a(&server, &key, "GetTask", json!({ "id": task_id, "historyLength": 1 }), "1.0")?;
    assert_eq!(got["result"]["id"], task_id.as_str());
    assert_eq!(got["result"]["history"].as_array().unwrap().len(), 1);
    assert_eq!(a2a(&server, &key, "CancelTask", json!({ "id": task_id }), "1.0")?["error"]["code"], -32002);
    let listed = a2a(&server, &key, "ListTasks", json!({ "contextId": context }), "1.0")?;
    assert_eq!(listed["result"]["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(a2a(&server, server.token(), "GetTask", json!({ "id": task_id }), "1.0")?["error"]["code"], -32001, "another user's task isn't found");

    // A follow-up in the same context carries the conversation.
    let follow = json!({ "message": { "messageId": "m2", "role": "ROLE_USER", "contextId": context, "parts": [{ "text": "And in the US?" }] } });
    assert_eq!(a2a(&server, &key, "SendMessage", follow, "1.0")?["result"]["task"]["contextId"], context.as_str());

    // 0.3 clients: their method names and shapes.
    let old = a2a(&server, &key, "message/send", json!({ "message": { "kind": "message", "messageId": "m3", "role": "user", "parts": [{ "kind": "text", "text": "How many orders?" }] } }), "")?;
    assert_eq!(old["result"]["kind"], "task", "{}", old);
    assert_eq!(old["result"]["status"]["state"], "completed");

    // Unsupported things say so.
    // Streaming: Server-Sent Events, each a JSON-RPC response with one StreamResponse.
    assert_eq!(card["capabilities"]["streaming"], true);
    let events = |method: &str, params: Value, version: &str| -> Result<Vec<Value>> {
        let res = server.request_as(Some(&key), Method::POST, "/a2a", Some(&json!({ "jsonrpc": "2.0", "id": 9, "method": method, "params": params })), &[("a2a-version", version)])?;
        assert_eq!(res.status, StatusCode::OK);
        assert!(res.headers["content-type"].to_str()?.starts_with("text/event-stream"));
        let text = res.body.as_str().unwrap_or_default().to_string();
        Ok(text.lines().filter_map(|l| l.strip_prefix("data:")).map(|d| serde_json::from_str::<Value>(d.trim()).unwrap()["result"].clone()).collect())
    };
    let stream = events("SendStreamingMessage", send.clone(), "1.0")?;
    assert_eq!(stream[0]["task"]["status"]["state"], "TASK_STATE_WORKING", "{:?}", stream);
    assert!(stream.iter().any(|e| e["statusUpdate"]["status"]["message"]["parts"][0]["text"] == "Looking things up: count in 'orders'"), "{:?}", stream);
    let artifact = stream.iter().find(|e| e.get("artifactUpdate").is_some()).expect("the answer");
    assert!(artifact["artifactUpdate"]["artifact"]["parts"][0]["text"].as_str().unwrap().contains("\"count\":3"), "{:?}", stream);
    let last = stream.last().unwrap();
    assert_eq!(last["statusUpdate"]["status"]["state"], "TASK_STATE_COMPLETED", "{:?}", stream);
    let streamed_task = stream[0]["task"]["id"].as_str().unwrap();
    assert_eq!(a2a(&server, &key, "GetTask", json!({ "id": streamed_task }), "1.0")?["result"]["status"]["state"], "TASK_STATE_COMPLETED", "kept for GetTask");
    let old_stream = events("message/stream", json!({ "message": { "kind": "message", "messageId": "m5", "role": "user", "parts": [{ "kind": "text", "text": "How many orders?" }] } }), "")?;
    assert_eq!(old_stream[0]["kind"], "task");
    let last = old_stream.last().unwrap();
    assert_eq!((last["kind"].clone(), last["final"].clone(), last["status"]["state"].clone()), (json!("status-update"), json!(true), json!("completed")), "{:?}", old_stream);
    assert_eq!(a2a(&server, &key, "SubscribeToTask", json!({ "id": streamed_task }), "1.0")?["error"]["code"], -32004);
    let image = json!({ "message": { "messageId": "m4", "role": "ROLE_USER", "parts": [{ "url": "https://example.com/x.png" }] } });
    assert_eq!(a2a(&server, &key, "SendMessage", image, "1.0")?["error"]["code"], -32005);
    assert_eq!(a2a(&server, &key, "SendMessage", send.clone(), "2.0")?["error"]["code"], -32009);

    // Without the chat permission: refused.
    let reader = server.user_with_roles("rita", json!([{ "name": "reader", "tessellations": ["orders"] }]))?;
    let res = server.request_as(Some(&reader), Method::POST, "/a2a", Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": send })), &[])?;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let audit = server.request(Method::GET, "/audit?action=a2a.message", None, &[])?;
    assert!(audit.body["events"].as_array().is_some_and(|e| e.len() >= 3), "{}", audit.body);
    Ok(())
}

/// A webhook receiver: each request's headers (lowercase names) and JSON body.
type Received = Arc<Mutex<Vec<(std::collections::HashMap<String, String>, Value)>>>;

fn webhook() -> Result<(u16, Received)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let received: Received = Arc::default();
    let record = received.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let mut headers = std::collections::HashMap::new();
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap_or(0);
                let h = h.trim_end().to_string();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                }
            }
            let length = headers.get("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).ok();
            record.lock().unwrap().push((headers, serde_json::from_slice(&body).unwrap_or(Value::Null)));
            let mut stream = stream;
            let _ = write!(stream, "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
        }
    });
    Ok((port, received))
}

fn wait_for(received: &Received, count: usize) -> Vec<(std::collections::HashMap<String, String>, Value)> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while received.lock().unwrap().len() < count && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    received.lock().unwrap().clone()
}

#[test]
fn a2a_push_notifications_post_finished_tasks_to_webhooks() -> Result<()> {
    let server = TestServer::start()?;
    let key = seed(&server)?;
    let (port, _seen) = mock_claude()?;
    configure(&server, json!({ "ai.provider": "anthropic", "ai.base_url": format!("http://127.0.0.1:{}", port), "ai.api_key": "sk-test-a2a-0001", "ai.chat.enabled": true, "a2a.enabled": true }))?;
    let (hook_port, received) = webhook()?;
    let hook = format!("http://127.0.0.1:{}/hook", hook_port);
    let anon = hexdb_tests::http().build()?;
    let card: Value = anon.get(server.url("/.well-known/agent-card.json")).send()?.json()?;
    assert_eq!(card["capabilities"]["pushNotifications"], true);

    // The card is signed (detached JWS over its RFC 8785 form) with the key its jku publishes.
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let signature = &card["signatures"][0];
    let protected = signature["protected"].as_str().unwrap();
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(protected)?)?;
    assert_eq!((header["alg"].as_str(), header["typ"].as_str()), (Some("EdDSA"), Some("JOSE")), "{}", header);
    let jku = header["jku"].as_str().unwrap();
    assert!(jku.ends_with("/.well-known/jwks.json"), "{}", jku);
    let jwks: Value = anon.get(server.url("/.well-known/jwks.json")).send()?.json()?;
    let jwk = jwks["keys"].as_array().unwrap().iter().find(|k| k["kid"] == header["kid"]).expect("the kid is published");
    let mut unsigned = card.clone();
    unsigned.as_object_mut().unwrap().remove("signatures");
    let input = format!("{}.{}", protected, URL_SAFE_NO_PAD.encode(hexdb_core::jws::canonical(&unsigned)));
    let public = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, URL_SAFE_NO_PAD.decode(jwk["x"].as_str().unwrap())?);
    assert!(public.verify(input.as_bytes(), &URL_SAFE_NO_PAD.decode(signature["signature"].as_str().unwrap())?).is_ok(), "the signature verifies");
    unsigned["name"] = json!("Not HexDB");
    let forged = format!("{}.{}", protected, URL_SAFE_NO_PAD.encode(hexdb_core::jws::canonical(&unsigned)));
    assert!(public.verify(forged.as_bytes(), &URL_SAFE_NO_PAD.decode(signature["signature"].as_str().unwrap())?).is_err());

    // Private addresses and plain HTTP are refused until a2a.push_private_networks allows them.
    let question = |config: Value| json!({ "message": { "messageId": "m1", "role": "ROLE_USER", "parts": [{ "text": "How many orders?" }] }, "configuration": config });
    let config = json!({ "url": hook, "token": "task-token", "authentication": { "scheme": "Bearer", "credentials": "hook-secret" } });
    let refused = a2a(&server, &key, "SendMessage", question(json!({ "taskPushNotificationConfig": config })), "1.0")?;
    assert_eq!(refused["error"]["code"], -32602, "{}", refused);
    assert!(refused["error"]["message"].as_str().unwrap().contains("a2a.push_private_networks"), "{}", refused);
    configure(&server, json!({ "a2a.push_private_networks": true }))?;

    // returnImmediately: the working task now, the finished one at the webhook.
    let started = a2a(&server, &key, "SendMessage", question(json!({ "taskPushNotificationConfig": config, "returnImmediately": true })), "1.0")?;
    assert_eq!(started["result"]["task"]["status"]["state"], "TASK_STATE_WORKING", "{}", started);
    let task_id = started["result"]["task"]["id"].as_str().unwrap().to_string();
    let posts = wait_for(&received, 1);
    assert_eq!(posts.len(), 1, "one notification");
    let (headers, body) = &posts[0];
    assert_eq!(body["task"]["id"], task_id.as_str());
    assert_eq!(body["task"]["status"]["state"], "TASK_STATE_COMPLETED", "{}", body);
    assert_eq!(headers["content-type"], "application/a2a+json");
    assert_eq!(headers["x-a2a-notification-token"], "task-token");
    assert_eq!(headers["authorization"], "Bearer hook-secret");
    let got = a2a(&server, &key, "GetTask", json!({ "id": task_id }), "1.0")?;
    assert_eq!(got["result"]["status"]["state"], "TASK_STATE_COMPLETED");

    // The configs: listed without credentials, got, deleted.
    let listed = a2a(&server, &key, "ListTaskPushNotificationConfigs", json!({ "taskId": task_id }), "1.0")?;
    let configs = listed["result"]["configs"].as_array().unwrap().clone();
    assert_eq!(configs.len(), 1, "{}", listed);
    assert!(!listed.to_string().contains("hook-secret"), "credentials aren't echoed");
    let config_id = configs[0]["id"].as_str().unwrap().to_string();
    let one = a2a(&server, &key, "GetTaskPushNotificationConfig", json!({ "taskId": task_id, "id": config_id }), "1.0")?;
    assert_eq!(one["result"]["url"], hook.as_str());
    let deleted = a2a(&server, &key, "DeleteTaskPushNotificationConfig", json!({ "taskId": task_id, "id": config_id }), "1.0")?;
    assert!(deleted["error"].is_null(), "{}", deleted);
    let listed = a2a(&server, &key, "ListTaskPushNotificationConfigs", json!({ "taskId": task_id }), "1.0")?;
    assert_eq!(listed["result"]["configs"], json!([]));

    // A webhook added to a finished task gets it at once, here through 0.3's method, in 0.3's form.
    let set = a2a(&server, &key, "tasks/pushNotificationConfig/set", json!({ "taskId": task_id, "pushNotificationConfig": { "url": hook } }), "0.3")?;
    assert_eq!(set["result"]["taskId"], task_id.as_str(), "{}", set);
    let posts = wait_for(&received, 2);
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[1].1["kind"], "task", "{}", posts[1].1);
    assert_eq!(posts[1].1["status"]["state"], "completed");

    // Someone else's task: not found.
    let other = server.user_with_roles("otto", json!([{ "name": "eu_reader", "tessellations": ["orders"] }]))?;
    let res = a2a(&server, &other, "CreateTaskPushNotificationConfig", json!({ "taskId": task_id, "url": hook }), "1.0")?;
    assert_eq!(res["error"]["code"], -32001, "{}", res);
    Ok(())
}
