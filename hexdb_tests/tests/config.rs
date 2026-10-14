//! The lattice configuration: versions, write-only secrets, validation,
//! per-hex storage overrides, history, rollback, the audit trail, and
//! propagation to every hex (including after a failover).

use anyhow::{bail, Result};
use hexdb_tests::{TestOptions, TestServer};
use reqwest::Method;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const SECRET: &str = "sk-test-DO-NOT-LEAK-7f3a9c";
const SECRET_2: &str = "sk-test-SECOND-KEY-b81e04";

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

fn get_config(server: &TestServer) -> Result<Value> {
    let res = server.request(Method::GET, "/config", None, &[])?;
    assert_eq!(res.status, 200, "{}", res.body);
    Ok(res.body)
}

fn put_config(server: &TestServer, body: Value) -> Result<hexdb_tests::ApiResponse> {
    server.request(Method::PUT, "/config", Some(&body), &[])
}

fn status_config(server: &TestServer) -> Result<Value> {
    Ok(server.request(Method::GET, "/status", None, &[])?.body["config"].clone())
}

fn assert_no_secret(text: &str, context: &str) {
    assert!(!text.contains(SECRET) && !text.contains(SECRET_2), "a secret leaked into {}", context);
}

#[test]
fn versions_secrets_validation_history_and_rollback() -> Result<()> {
    let mut server = TestServer::start()?;

    // Nothing saved yet: defaults, and the schema to build forms from.
    let view = get_config(&server)?;
    assert_eq!(view["version"], 0);
    assert!(view["schema"].as_array().unwrap().iter().any(|d| d["key"] == "ai.api_key" && d["secret"] == true));
    assert_eq!(status_config(&server)?["state"], "default");

    // Settings and a secret in one version.
    let res = put_config(&server, json!({ "changes": { "ai.provider": "anthropic", "ai.api_key": SECRET, "ai.tagging.send": ["image", "metadata"] }, "comment": "Use Claude" }))?;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_no_secret(&res.body.to_string(), "the PUT response");
    assert_eq!(res.body["version"], 1);
    assert_eq!(res.body["changes"]["secrets_set"], json!(["ai.api_key"]));
    assert_eq!(res.body["lattice"]["ai.provider"], "anthropic");
    assert_eq!(res.body["lattice"]["ai.tagging.send"], json!(["metadata", "image"]), "normalized to the schema's order");
    let key_info = res.body["lattice"]["ai.api_key"].clone();
    assert_eq!(key_info["set"], true);
    assert_eq!(key_info["fingerprint"].as_str().unwrap().len(), 16);
    let v1_sha = res.body["sha256"].as_str().unwrap().to_string();

    // Applied on this hex at once, with the same checksum.
    let applied = status_config(&server)?;
    assert_eq!(applied["version"], 1);
    assert_eq!(applied["state"], "applied");
    assert_eq!(applied["sha256"], v1_sha);

    // Bad requests change nothing.
    for (body, why) in [
        (json!({ "changes": { "ai.provider": "skynet" } }), "not a choice"),
        (json!({ "changes": { "ai.nope": 1 } }), "unknown key"),
        (json!({ "changes": { "ai.base_url": "file:///etc/passwd" } }), "not an http URL"),
        (json!({ "changes": { "ai.monthly_token_budget": -5 } }), "below the minimum"),
        (json!({ "changes": { "ai.provider": "openai" }, "hex": "Elsewhere" }), "not a storage setting"),
        (json!({ "changes": {} }), "no changes"),
        (json!({ "changes": { "ai.api_key": "" } }), "empty secret"),
    ] {
        let res = put_config(&server, body)?;
        assert_eq!(res.status, 400, "{}: {}", why, res.body);
    }
    let res = put_config(&server, json!({ "changes": { "ai.model": "x" }, "base_version": 0 }))?;
    assert_eq!(res.status, 409, "stale base version: {}", res.body);

    // Only admins see or change it.
    let reader = server.user_with_roles("rita", json!([{ "name": "reader", "tessellations": ["*"] }]))?;
    for (method, path, body) in [
        (Method::GET, "/config", None),
        (Method::GET, "/config/history", None),
        (Method::PUT, "/config", Some(json!({ "changes": { "ai.model": "x" } }))),
        (Method::POST, "/config/rollback", Some(json!({ "version": 1 }))),
    ] {
        let res = server.request_as(Some(&reader), method, path, body.as_ref(), &[])?;
        assert_eq!(res.status, 403, "{}: {}", path, res.body);
    }

    // Saving the same values again is not a new version.
    let res = put_config(&server, json!({ "changes": { "ai.provider": "anthropic" } }))?;
    assert_eq!(res.body["version"], 1, "{}", res.body);

    // A storage override for one hex.
    let res = put_config(&server, json!({ "changes": { "file_storage.backend": "s3", "file_storage.s3.bucket": "far-away", "file_storage.s3.secret_access_key": SECRET_2 }, "hex": "Elsewhere", "base_version": 1 }))?;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(res.body["version"], 2);
    assert_eq!(res.body["overrides"]["Elsewhere"]["file_storage.backend"], "s3");
    assert_eq!(res.body["overrides"]["Elsewhere"]["file_storage.s3.secret_access_key"]["set"], true);
    assert_no_secret(&res.body.to_string(), "the override response");
    assert_eq!(status_config(&server)?["overridden"], json!([]), "the override is for another hex");

    // Replace the key, then look at the history.
    let res = put_config(&server, json!({ "changes": { "ai.api_key": SECRET_2, "ai.provider": "openai" } }))?;
    assert_eq!(res.body["version"], 3);
    assert_ne!(res.body["lattice"]["ai.api_key"]["fingerprint"], key_info["fingerprint"], "a different key, a different fingerprint");
    let history = server.request(Method::GET, "/config/history", None, &[])?.body;
    assert_no_secret(&history.to_string(), "the history");
    assert_eq!(history["total"], 3);
    let versions = history["versions"].as_array().unwrap();
    assert_eq!(versions[0]["version"], 3);
    assert_eq!(versions[0]["diff"]["changed"], json!(["ai.api_key", "ai.provider"]));
    assert_eq!(versions[1]["diff"]["changed"], json!(["hex:Elsewhere/file_storage.backend", "hex:Elsewhere/file_storage.s3.bucket", "hex:Elsewhere/file_storage.s3.secret_access_key"]));
    assert_eq!(versions[2]["comment"], "Use Claude");
    let one = server.request(Method::GET, "/config/versions/1", None, &[])?;
    assert_eq!(one.body["valid"], true);
    assert_eq!(one.body["sha256"], v1_sha);
    assert_no_secret(&one.body.to_string(), "a version");
    assert_eq!(server.request(Method::GET, "/config/versions/99", None, &[])?.status, 404);

    // Roll back to version 1: a new version with version 1's contents, its key included.
    let res = server.request(Method::POST, "/config/rollback", Some(&json!({ "version": 1, "comment": "bad key" })), &[])?;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(res.body["version"], 4);
    assert_eq!(res.body["lattice"]["ai.provider"], "anthropic");
    assert_eq!(res.body["lattice"]["ai.api_key"]["fingerprint"], key_info["fingerprint"]);
    assert_eq!(res.body["overrides"], json!({}));
    let history = server.request(Method::GET, "/config/history?limit=1", None, &[])?.body;
    assert_eq!(history["versions"][0]["rollback_of"], 1);

    // Clearing a secret.
    let res = put_config(&server, json!({ "changes": { "ai.api_key": null } }))?;
    assert_eq!(res.body["changes"]["secrets_cleared"], json!(["ai.api_key"]));
    assert!(res.body["lattice"].get("ai.api_key").is_none());

    // Every change is audited, without values of secrets.
    let audit = server.request(Method::GET, "/audit?action=config.", None, &[])?.body;
    let actions: Vec<&str> = audit["events"].as_array().unwrap().iter().filter_map(|e| e["action"].as_str()).collect();
    assert_eq!(actions.iter().filter(|a| **a == "config.update").count(), 4, "{:?}", actions);
    assert_eq!(actions.iter().filter(|a| **a == "config.rollback").count(), 1, "{:?}", actions);
    assert_no_secret(&audit.to_string(), "the audit trail");

    // It survives a restart, and the secrets never reached the log.
    server.restart()?;
    let applied = status_config(&server)?;
    assert_eq!(applied["version"], 5);
    assert_eq!(applied["state"], "applied");
    assert_eq!(get_config(&server)?["lattice"]["ai.provider"], "anthropic");
    assert_no_secret(&std::fs::read_to_string(server.log_path()).unwrap_or_default(), "the server log");
    Ok(())
}

fn pair(ports: [u16; 2], i: usize, ram_mb: u64) -> TestOptions {
    TestOptions {
        lattice: Some(format!("config-{}", ports[0])),
        discovery_port: Some(ports[i]),
        peers: vec![format!("127.0.0.1:{}", ports[1 - i])],
        ram_mb: Some(ram_mb),
        discovery_interval_seconds: Some(1),
        ..Default::default()
    }
}

fn role(server: &TestServer) -> Result<String> {
    Ok(server.request(Method::GET, "/status", None, &[])?.body["hex_type"].as_str().unwrap_or_default().to_string())
}

#[test]
fn every_hex_applies_the_config_and_it_survives_failover() -> Result<()> {
    let ports = [TestServer::free_port()?, TestServer::free_port()?];
    let mut overseer = TestServer::start_with(pair(ports, 0, 4096))?;
    assert_eq!(role(&overseer)?, "Overseer");
    let res = put_config(&overseer, json!({ "changes": { "ai.provider": "anthropic", "ai.api_key": SECRET } }))?;
    assert_eq!(res.status, 200, "{}", res.body);
    let sha = res.body["sha256"].clone();

    // A hex that joins later gets it with its first sync and checks it.
    let replica = TestServer::start_with(pair(ports, 1, 1024))?;
    wait_until(Duration::from_secs(30), "the replica to apply version 1", || Ok(status_config(&replica)?["sha256"] == sha))?;
    assert_eq!(status_config(&replica)?["state"], "applied");

    // Its own storage override applies there only; changes sent to the replica go to the Overseer.
    let replica_name = replica.request(Method::GET, "/status", None, &[])?.body["name"].as_str().unwrap().to_string();
    let res = put_config(&replica, json!({ "changes": { "file_storage.backend": "azure", "file_storage.azure.account": "near" }, "hex": replica_name }))?;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(res.body["version"], 2);
    wait_until(Duration::from_secs(30), "the replica to apply version 2", || Ok(status_config(&replica)?["version"] == 2))?;
    assert_eq!(status_config(&replica)?["overridden"], json!(["file_storage.azure.account", "file_storage.backend"]));
    assert_eq!(status_config(&overseer)?["overridden"], json!([]));

    // The Overseer sees which version each hex runs.
    wait_until(Duration::from_secs(20), "the lattice view to show the replica on version 2", || {
        let view = get_config(&overseer)?;
        Ok(view["hexes"].as_array().unwrap().iter().any(|h| h["is_self"] == false && h["config"]["version"] == 2 && h["config"]["state"] == "applied"))
    })?;

    // The Overseer dies: the replica takes over with the configuration and can change it.
    overseer.kill();
    wait_until(Duration::from_secs(30), "failover", || Ok(role(&replica)? == "Overseer"))?;
    let view = get_config(&replica)?;
    assert_eq!(view["version"], 2);
    assert_eq!(view["lattice"]["ai.api_key"]["set"], true);
    let res = put_config(&replica, json!({ "changes": { "ai.model": "a-model" }, "base_version": 2 }))?;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(res.body["version"], 3);
    assert_eq!(status_config(&replica)?["version"], 3);

    // The old Overseer comes back as a replica and catches up.
    overseer.launch()?;
    wait_until(Duration::from_secs(40), "the old Overseer to apply version 3", || Ok(status_config(&overseer)?["version"] == 3))?;
    assert_eq!(status_config(&overseer)?["sha256"], get_config(&replica)?["sha256"]);
    for server in [&overseer, &replica] {
        assert_no_secret(&std::fs::read_to_string(server.log_path()).unwrap_or_default(), "a server log");
    }
    Ok(())
}

#[test]
fn demo_mode_lets_everyone_look_but_only_admins_change() -> Result<()> {
    for demo in [false, true] {
        let server = TestServer::start_with(TestOptions { security_toml: format!("demo_mode = {}", demo), ..Default::default() })?;
        let res = put_config(&server, json!({ "changes": { "ai.provider": "anthropic", "ai.api_key": SECRET } }))?;
        assert_eq!(res.status, 200, "{}", res.body);
        let visitor = server.user_with_roles("visitor", json!([{ "name": "reader", "tessellations": ["*"] }]))?;
        let me = server.request_as(Some(&visitor), Method::GET, "/auth/me", None, &[])?;
        assert_eq!(me.body["demo_mode"], demo);
        // Viewing: only in demo mode, and never a secret.
        for path in ["/config", "/config/history", "/config/versions/1", "/ai", "/ai/estimate"] {
            let res = server.request_as(Some(&visitor), Method::GET, path, None, &[])?;
            assert_eq!(res.status, if demo { 200 } else { 403 }, "{} with demo_mode = {}: {}", path, demo, res.body);
            assert_no_secret(&res.body.to_string(), path);
        }
        // Changing, testing and spending: administrators only, demo mode or not.
        for (method, path, body) in [
            (Method::PUT, "/config", Some(json!({ "changes": { "ai.model": "x" } }))),
            (Method::POST, "/config/rollback", Some(json!({ "version": 1 }))),
            (Method::POST, "/config/storage-test", None),
            (Method::POST, "/ai/test", None),
        ] {
            let res = server.request_as(Some(&visitor), method, path, body.as_ref(), &[])?;
            assert_eq!(res.status, 403, "{} with demo_mode = {}: {}", path, demo, res.body);
        }
        // Not even signed in: nothing.
        assert_eq!(server.request_as(None, Method::GET, "/config", None, &[])?.status, 401);
    }
    Ok(())
}
