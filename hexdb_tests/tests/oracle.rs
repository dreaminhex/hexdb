//! The Oracle: a hex with `identity.role = "oracle"` never leads, keeps no
//! user data, and answers the lattice's AI questions, reading what it needs
//! from a hex that holds the data, as the asking user.

use anyhow::{bail, Result};
use hexdb_tests::{TestOptions, TestServer};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn wait_until(timeout: Duration, what: &str, mut check: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if check()? {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {}", what);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn status(server: &TestServer) -> Result<Value> {
    Ok(server.request(Method::GET, "/status", None, &[])?.body)
}

/// Two hexes: one that holds the data and leads, and an Oracle.
fn lattice() -> Result<(TestServer, TestServer)> {
    let ports = [TestServer::free_port()?, TestServer::free_port()?];
    let lattice = format!("oracle-{}", ports[0]);
    let options = |i: usize, role: &str| TestOptions {
        lattice: Some(lattice.clone()),
        discovery_port: Some(ports[i]),
        peers: vec![format!("127.0.0.1:{}", ports[1 - i])],
        role: Some(role.into()),
        discovery_interval_seconds: Some(1),
        ..Default::default()
    };
    let data = TestServer::start_with(options(0, "auto"))?;
    // The Oracle has the most memory: it still never leads.
    let oracle = TestServer::start_with(TestOptions { ram_mb: Some(4096), ..options(1, "oracle") })?;
    wait_until(Duration::from_secs(30), "the Oracle to join", || {
        let s = status(&data)?;
        Ok(s["network"]["lattice"]["hexes"].as_array().is_some_and(|h| h.iter().any(|h| h["role"] == "Oracle" && h["status"] == "active")))
    })?;
    Ok((data, oracle))
}

fn mcp_tool(server: &TestServer, key: &str, name: &str, arguments: Value) -> Result<(Value, Option<String>)> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": arguments } });
    let res = server.request_as(Some(key), Method::POST, "/mcp", Some(&body), &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let to = res.headers.get("x-hexdb-forwarded-to").and_then(|v| v.to_str().ok()).map(String::from);
    Ok((res.body, to))
}

#[test]
fn the_oracle_answers_ai_questions_from_the_data_hexes_and_keeps_none() -> Result<()> {
    let (data, oracle) = lattice()?;
    assert_eq!(status(&data)?["hex_type"], "Overseer");
    let oracle_status = status(&oracle)?;
    assert_eq!(oracle_status["hex_type"], "Oracle");
    let oracle_name = oracle_status["hex_name"].as_str().or(oracle_status["name"].as_str()).unwrap_or_default().to_string();

    for (i, region) in ["eu", "eu", "us"].iter().enumerate() {
        data.insert("orders", &json!({ "n": i, "region": region, "secret": format!("card-{}", i) }))?;
    }
    let role = json!({ "name": "eu_reader", "permissions": ["read"], "restrictions": { "orders": { "filter": { "region": "eu" }, "hide": ["secret"] } } });
    assert!(data.request(Method::POST, "/roles", Some(&role), &[])?.status.is_success());
    let key = data.user_with_roles("emma", json!([{ "name": "eu_reader", "tessellations": ["orders"] }]))?;

    // An MCP question asked of the data hex is answered by the Oracle, which
    // read the orders from the data hex as emma: her row filter and hidden field apply.
    let mut answer = (Value::Null, None);
    wait_until(Duration::from_secs(30), "the Oracle to answer as emma", || {
        answer = mcp_tool(&data, &key, "query", json!({ "tessellation": "orders" }))?;
        Ok(answer.1.is_some() && answer.0["result"]["isError"] != true)
    })?;
    let (reply, forwarded_to) = answer;
    if !oracle_name.is_empty() {
        assert_eq!(forwarded_to.as_deref(), Some(oracle_name.as_str()));
    }
    let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_default();
    let found: Value = serde_json::from_str(text)?;
    let docs = found["documents"].as_array().cloned().unwrap_or_default();
    assert_eq!(docs.len(), 2, "{}", text);
    assert!(docs.iter().all(|d| d["region"] == "eu" && d.get("secret").is_none()), "{}", text);

    // Asked directly, the Oracle answers the same way.
    let (reply, _) = mcp_tool(&oracle, &key, "count", json!({ "tessellation": "orders" }))?;
    assert!(reply["result"]["content"][0]["text"].as_str().unwrap_or_default().contains("2"), "{}", reply);

    // It keeps no user data: none of the slices, and no orders of its own;
    // a REST read made there is answered by the data hex.
    let s = status(&oracle)?;
    assert_eq!(s["replication"]["slices_held_count"], 0, "{}", s["replication"]);
    let orders = s["tessellations"].as_array().and_then(|t| t.iter().find(|t| t["name"] == "orders").cloned());
    assert!(orders.as_ref().is_none_or(|t| t["document_count"] == 0), "{:?}", orders);
    let res = oracle.request(Method::POST, "/orders/_query", Some(&json!({ "filter": {} })), &[])?;
    assert_eq!(res.body["documents"].as_array().map(|d| d.len()), Some(3), "{}", res.body);

    // Without the Oracle, the data hex answers AI questions itself.
    drop(oracle);
    let (reply, forwarded_to) = mcp_tool(&data, &key, "count", json!({ "tessellation": "orders" }))?;
    assert_eq!(forwarded_to, None, "{}", reply);
    assert!(reply["result"]["content"][0]["text"].as_str().unwrap_or_default().contains("2"), "{}", reply);
    Ok(())
}
