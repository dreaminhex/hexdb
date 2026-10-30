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
            let reply = if let Some(result) = results.first() {
                json!({ "content": [{ "type": "text", "text": format!("The tool said: {}", result) }], "stop_reason": "end_turn", "usage": { "input_tokens": 40, "output_tokens": 10 } })
            } else if body["tools"].is_array() {
                json!({ "content": [{ "type": "text", "text": "Let me count." }, { "type": "tool_use", "id": "call_1", "name": "count", "input": { "tessellation": "orders" } }], "stop_reason": "tool_use", "usage": { "input_tokens": 30, "output_tokens": 8 } })
            } else {
                json!({ "content": [{ "type": "text", "text": "No tools left." }], "stop_reason": "end_turn", "usage": { "input_tokens": 5, "output_tokens": 3 } })
            };
            let text = reply.to_string();
            let mut stream = stream;
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", text.len(), text);
        }
    });
    Ok((port, seen))
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

    // Usage is counted as the chat feature, and each question is audited.
    let usage = server.request(Method::GET, "/ai", None, &[])?.body;
    assert!(usage["usage"]["features"]["chat"]["calls"].as_u64().unwrap() >= 6, "{}", usage);
    let audit = server.request(Method::GET, "/audit?action=ai.chat", None, &[])?.body;
    assert_eq!(audit["events"].as_array().unwrap().len(), 3, "{}", audit);
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
