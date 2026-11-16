//! Review regression tests. Assertions describe the intended safe behavior.
use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};

fn setup_vault(server: &TestServer) -> Result<String> {
    let r = server.request(Method::POST, "/tessellations", Some(&json!({"name":"accounts"})), &[])?;
    assert!(r.status.is_success(), "{}", r.body);
    let r = server.request(Method::POST, "/tessellations/accounts/schemas", Some(&json!({"fields":{"card":{"type":"string","tokenize":"vault"}},"additional_fields":true})), &[])?;
    assert!(r.status.is_success(), "{}", r.body);
    server.insert("accounts", &json!({"tenant":"B", "card":"4111111111111111"}))
}

fn role(server: &TestServer, name: &str, permissions: Value, restriction: Value) -> Result<String> {
    let r = server.request(Method::POST, "/roles", Some(&json!({"name":name,"permissions":permissions,"restrictions":{"accounts":restriction}})), &[])?;
    assert!(r.status.is_success(), "{}", r.body);
    server.user_with_roles(name, json!([{"name":name,"tessellations":["accounts"]}]))
}

#[test]
fn review_detokenize_respects_tenant_rows() -> Result<()> {
    let server = TestServer::start()?;
    let id = setup_vault(&server)?;
    let token = server.get_doc("accounts", &id)?.unwrap()["card"].clone();
    let user = role(&server, "tenant-a", json!(["read","detokenize"]), json!({"filter":{"tenant":"A"}}))?;
    assert_eq!(server.request_as(Some(&user), Method::GET, &format!("/accounts/{id}"), None, &[])?.status, StatusCode::NOT_FOUND);
    let r = server.request_as(Some(&user), Method::POST, "/tessellations/accounts/detokenize", Some(&json!({"tokens":[token]})), &[])?;
    assert!(!r.body.to_string().contains("4111111111111111"), "A token from tenant B was decrypted for tenant A: {}", r.body);
    Ok(())
}

#[test]
fn review_vault_bulk_returns_only_user_document_ids() -> Result<()> {
    let server = TestServer::start()?;
    setup_vault(&server)?;
    let r = server.request(Method::POST, "/accounts/_bulk", Some(&json!([{"card":"5500005555555559"}])), &[])?;
    assert!(r.status.is_success(), "{}", r.body);
    assert_eq!(r.body["count"], 1, "One inserted account must return one account ID: {}", r.body);
    Ok(())
}

#[test]
fn review_vault_write_works_with_tenant_restriction() -> Result<()> {
    let server = TestServer::start()?;
    setup_vault(&server)?;
    let user = role(&server, "tenant-writer", json!(["read","write"]), json!({"filter":{"tenant":"A"}}))?;
    let r = server.request_as(Some(&user), Method::POST, "/accounts", Some(&json!({"tenant":"A","card":"5500005555555559"})), &[])?;
    assert_eq!(r.status, StatusCode::CREATED, "Authorized vault write must not check its internal _vault entry as an account: {}", r.body);
    Ok(())
}

#[test]
fn review_restored_document_keeps_its_vault_value() -> Result<()> {
    let server = TestServer::start()?;
    let id = setup_vault(&server)?;
    let token = server.get_doc("accounts", &id)?.unwrap()["card"].as_str().unwrap().to_string();
    let r = server.request(Method::DELETE, &format!("/accounts/{id}"), None, &[])?;
    assert!(r.status.is_success(), "{}", r.body);
    let batch = r.headers.get("x-hexdb-delete-batch").unwrap().to_str()?.to_string();
    let r = server.request(Method::POST, "/compact", None, &[])?;
    assert!(r.status.is_success(), "{}", r.body);
    let r = server.request(Method::POST, &format!("/deletes/{batch}/restore"), None, &[])?;
    assert!(r.status.is_success(), "{}", r.body);
    assert!(server.get_doc("accounts", &id)?.is_some());
    let r = server.request(Method::POST, "/tessellations/accounts/detokenize", Some(&json!({"tokens":[token]})), &[])?;
    assert_eq!(r.body["values"][&token], "4111111111111111", "Restoring a retained document must restore its vault value too: {}", r.body);
    Ok(())
}

#[test]
fn review_detokenize_respects_hidden_parent() -> Result<()> {
    let server = TestServer::start()?;
    let r = server.request(Method::POST, "/tessellations", Some(&json!({"name":"accounts"})), &[])?;
    assert!(r.status.is_success(), "{}", r.body);
    let r = server.request(Method::POST, "/tessellations/accounts/schemas", Some(&json!({"fields":{"profile.card":{"type":"string","tokenize":"vault"}},"additional_fields":true})), &[])?;
    assert!(r.status.is_success(), "{}", r.body);
    let id = server.insert("accounts", &json!({"profile":{"card":"4111111111111111"}}))?;
    let token = server.get_doc("accounts", &id)?.unwrap()["profile"]["card"].clone();
    let user = role(&server, "hidden-parent", json!(["read","detokenize"]), json!({"hide":["profile"]}))?;
    let r = server.request_as(Some(&user), Method::GET, &format!("/accounts/{id}"), None, &[])?;
    assert!(r.body.get("profile").is_none(), "{}", r.body);
    let r = server.request_as(Some(&user), Method::POST, "/tessellations/accounts/detokenize", Some(&json!({"tokens":[token]})), &[])?;
    assert!(!r.body.to_string().contains("4111111111111111"), "Hidden parent was bypassed: {}", r.body);
    Ok(())
}

#[test]
fn review_key_rotation_preserves_token_lookups() -> Result<()> {
    let mut server = TestServer::start()?;
    let id = setup_vault(&server)?;
    server.stop()?;
    server.set_options(hexdb_tests::TestOptions {
        encryption_key: Some("base64:BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=".into()),
        previous_encryption_keys: vec![hexdb_tests::TEST_ENCRYPTION_KEY.into()],
        ..Default::default()
    });
    server.launch()?;
    assert!(server.get_doc("accounts", &id)?.is_some(), "Stored data remains readable with the previous key");
    let r = server.request(Method::POST, "/accounts/_query", Some(&json!({"filter":{"card":"4111111111111111"}})), &[])?;
    assert_eq!(r.body["documents"].as_array().map(Vec::len), Some(1), "Rotating keys must not break real-value equality lookup on a retained token: {}", r.body);
    Ok(())
}

#[test]
fn review_idempotent_retry_still_requires_replica_acks() -> Result<()> {
    let server = TestServer::start_with(hexdb_tests::TestOptions {
        extra_toml: "\n[replication]\nmin_acks = 1\nack_timeout_ms = 100\n".into(),
        ..Default::default()
    })?;
    let body = json!({"n":1});
    let headers = [("Idempotency-Key", "review-replica-ack")];
    let first = server.request(Method::POST, "/orders", Some(&body), &headers)?;
    assert_eq!(first.error_code(), Some("replication_timeout"), "{}", first.body);
    let retry = server.request(Method::POST, "/orders", Some(&body), &headers)?;
    assert_eq!(retry.status, StatusCode::SERVICE_UNAVAILABLE, "No replica exists, but retry acknowledged a synchronously replicated write: {}", retry.body);
    Ok(())
}

// Review of 2.5.5 (November 2026).

/// H255-01: two grants with different row filters. The tenant-B grant must not
/// uncover a field the tenant-A grant hides in tenant A's documents, over REST,
/// queries, SQL or the change feed; and the field can't be used to probe them.
#[test]
fn review255_role_fields_are_scoped_to_rows_the_role_allows() -> Result<()> {
    let s = TestServer::start()?;
    let a = s.insert("accounts", &json!({"tenant":"A","secret":"SYNTHETIC_A_SECRET"}))?;
    let b = s.insert("accounts", &json!({"tenant":"B","secret":"SYNTHETIC_B_VALUE"}))?;
    for (name, restriction) in [("a-redacted", json!({"filter":{"tenant":"A"},"hide":["secret"]})), ("b-full", json!({"filter":{"tenant":"B"}}))] {
        let r = s.request(Method::POST, "/roles", Some(&json!({"name":name,"permissions":["read","write"],"restrictions":{"accounts":restriction}})), &[])?;
        assert!(r.status.is_success(), "{}", r.body);
    }
    let single = s.user_with_roles("single", json!([{"name":"a-redacted","tessellations":["accounts"]}]))?;
    let both = s.user_with_roles("both", json!([{"name":"a-redacted","tessellations":["accounts"]},{"name":"b-full","tessellations":["accounts"]}]))?;
    let get = |who: &str, id: &str| s.request_as(Some(who), Method::GET, &format!("/accounts/{}", id), None, &[]);
    assert!(!get(&single, &a)?.body.to_string().contains("SYNTHETIC_A_SECRET"));
    assert!(!get(&both, &a)?.body.to_string().contains("SYNTHETIC_A_SECRET"), "A grant restricted to tenant B exposed tenant A's protected field");
    // The tenant-B grant still shows tenant B's field.
    assert_eq!(get(&both, &b)?.body["secret"], json!("SYNTHETIC_B_VALUE"));

    let listed = s.request_as(Some(&both), Method::POST, "/accounts/_query", Some(&json!({"filter":{}})), &[])?;
    assert_eq!(listed.body["documents"].as_array().map(Vec::len), Some(2), "{}", listed.body);
    assert!(!listed.body.to_string().contains("SYNTHETIC_A_SECRET"), "{}", listed.body);
    let probe = s.request_as(Some(&both), Method::POST, "/accounts/_query", Some(&json!({"filter":{"secret":"SYNTHETIC_A_SECRET"}})), &[])?;
    assert_eq!(probe.status, StatusCode::FORBIDDEN, "A query on a field hidden in tenant A's rows can probe them: {}", probe.body);
    let sql = s.request_as(Some(&both), Method::POST, "/sql", Some(&json!({"sql":"SELECT * FROM accounts"})), &[])?;
    assert!(!sql.body.to_string().contains("SYNTHETIC_A_SECRET"), "{}", sql.body);
    let feed = s.request_as(Some(&both), Method::GET, "/changes?after=0&limit=100", None, &[])?;
    assert!(feed.status.is_success(), "{}", feed.body);
    assert!(!feed.body.to_string().contains("SYNTHETIC_A_SECRET"), "{}", feed.body);

    // Writes follow the document's rows too.
    let w = s.request_as(Some(&both), Method::PATCH, &format!("/accounts/{}", b), Some(&json!({"secret":"changed"})), &[])?;
    assert!(w.status.is_success(), "{}", w.body);
    let w = s.request_as(Some(&both), Method::PATCH, &format!("/accounts/{}", a), Some(&json!({"secret":"changed"})), &[])?;
    assert_eq!(w.status, StatusCode::FORBIDDEN, "{}", w.body);
    let w = s.request_as(Some(&both), Method::PATCH, &format!("/accounts/{}", b), Some(&json!({"tenant":"A","secret":"moved"})), &[])?;
    assert_eq!(w.status, StatusCode::FORBIDDEN, "Moving a document into tenant A with a hidden field set: {}", w.body);
    Ok(())
}

/// H255-02: a stream reader never sees a destination's credentials.
#[test]
fn review255_stream_reader_cannot_read_webhook_credentials() -> Result<()> {
    let s = TestServer::start()?;
    let r = s.request(Method::PUT, "/config", Some(&json!({"changes":{"streams.private_networks":true}})), &[])?;
    assert!(r.status.is_success(), "{}", r.body);
    let r = s.request(Method::POST, "/streams", Some(&json!({"name":"jobs","destinations":[{"url":"http://127.0.0.1:9/hook?key=SYNTHETIC_QUERY_SECRET","headers":{"Authorization":"Bearer SYNTHETIC_WEBHOOK_SECRET"}}]})), &[])?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let reader = s.user_with_roles("limited", json!([{"name":"reader","tessellations":["stream:jobs"]}]))?;
    let a = s.request_as(Some(&reader), Method::GET, "/streams", None, &[])?;
    let b = s.request_as(Some(&reader), Method::GET, "/streams/jobs", None, &[])?;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(b.status, StatusCode::OK);
    let both = format!("{} {}", a.body, b.body);
    assert!(!both.contains("SYNTHETIC_WEBHOOK_SECRET") && !both.contains("SYNTHETIC_QUERY_SECRET"), "Read-only stream user received the destination's credentials: {}", both);
    Ok(())
}

/// H255-03: a reader can't move a destination's (or source's) internal cursor.
#[test]
fn review255_reader_cannot_overwrite_internal_delivery_cursor() -> Result<()> {
    let s = TestServer::start()?;
    let r = s.request(Method::PUT, "/config", Some(&json!({"changes":{"streams.private_networks":true}})), &[])?;
    assert!(r.status.is_success(), "{}", r.body);
    let r = s.request(Method::POST, "/streams", Some(&json!({"name":"jobs","destinations":[{"url":"http://127.0.0.1:9/hook"}]})), &[])?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let reader = s.user_with_roles("limited", json!([{"name":"reader","tessellations":["stream:jobs"]}]))?;
    for group in ["__destination:0", "__source:0"] {
        let r = s.request_as(Some(&reader), Method::POST, &format!("/streams/jobs/groups/{}/commit", group), Some(&json!({"offset":"7ZZZZZZZZZZZZZZZZZZZZZZZZZ"})), &[])?;
        assert!(r.status.is_client_error(), "A reader overwrote the internal cursor {}: status {}", group, r.status);
    }
    // Nor skip a group of its own past messages that don't exist yet.
    let r = s.request_as(Some(&reader), Method::POST, "/streams/jobs/groups/mine/commit", Some(&json!({"offset":"7ZZZZZZZZZZZZZZZZZZZZZZZZZ"})), &[])?;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    Ok(())
}

/// H255-07: a stream manager can't make the server POST to loopback or private addresses
/// unless an administrator allows private networks.
#[test]
fn review255_stream_owner_cannot_post_to_loopback_by_default() -> Result<()> {
    let s = TestServer::start()?;
    let owner = s.user_with_roles("owner", json!([{"name":"owner","tessellations":["stream:jobs"]}]))?;
    for url in ["http://127.0.0.1:9/internal", "http://localhost:9/internal", "http://[::1]:9/internal", "http://[::ffff:127.0.0.1]:9/internal", "http://169.254.169.254/latest", "http://10.0.0.1/x", "http://u:p@8.8.8.8/x"] {
        let r = s.request_as(Some(&owner), Method::POST, "/streams", Some(&json!({"name":"jobs","destinations":[{"url":url}]})), &[])?;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}: {}", url, r.body);
    }
    // Only an administrator can allow them.
    let r = s.request_as(Some(&owner), Method::PUT, "/config", Some(&json!({"changes":{"streams.private_networks":true}})), &[])?;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    Ok(())
}

/// H255-10: a de-duplication key outlives a restart (within its five minutes).
#[test]
fn review255_queue_dedupe_survives_restart() -> Result<()> {
    let mut s = TestServer::start()?;
    let r = s.request(Method::POST, "/queues", Some(&json!({"name":"jobs"})), &[])?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let body = json!({"messages":[{"body":"synthetic-job","dedupe_key":"same-job"}]});
    let first = s.request(Method::POST, "/queues/jobs/messages", Some(&body), &[])?;
    assert_eq!(first.status, StatusCode::CREATED);
    assert!(first.body["ids"][0].is_string());
    s.restart()?;
    let second = s.request(Method::POST, "/queues/jobs/messages", Some(&body), &[])?;
    assert_eq!(second.status, StatusCode::CREATED);
    assert!(second.body["ids"][0].is_null(), "Restart forgot a de-duplication key before its five-minute expiry");
    Ok(())
}

/// H255-08: a script whose request is cancelled (the request timeout) loses its session at once.
#[test]
fn review255_script_cancellation_revokes_temporary_session() -> Result<()> {
    let mut s = TestServer::start_with(hexdb_tests::TestOptions { extra_toml: "\n[limits]\nrequest_timeout_seconds = 2\n".into(), ..Default::default() })?;
    let temp = tempfile::tempdir()?;
    let token_file = temp.path().join("synthetic-session.txt");
    let code = "import sys,json,os,time\np=json.load(sys.stdin)['params']\nwith open(p['path'],'w') as f: f.write(os.environ['HEXDB_TOKEN'])\ntime.sleep(12)\nprint('{}')";
    let r = s.request(Method::POST, "/functions", Some(&json!({"name":"waiter","kind":"script","runtime":"python","code":code,"timeout_seconds":30,"params":[{"name":"path","type":"string","required":true}]})), &[])?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let reader = s.user_with_roles("limited", json!([{"name":"reader","tessellations":["orders"]}]))?;
    let r = s.request_as(Some(&reader), Method::POST, "/functions/waiter/run", Some(&json!({"params":{"path":token_file}})), &[])?;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "expected request cancellation: {}", r.body);
    let token = std::fs::read_to_string(&token_file)?;
    let check = s.request_as(Some(&token), Method::GET, "/auth/me", None, &[])?;
    assert_eq!(check.status, StatusCode::UNAUTHORIZED, "The canceled script's temporary session still authenticates");
    // Revoked lattice-wide (a durable record) within a few seconds, not just refused in this
    // process's memory: still refused after a restart.
    std::thread::sleep(std::time::Duration::from_secs(3));
    s.restart()?;
    let check = s.request_as(Some(&token), Method::GET, "/auth/me", None, &[])?;
    assert_eq!(check.status, StatusCode::UNAUTHORIZED, "The canceled script's session authenticates again after a restart");
    Ok(())
}

/// H255-09: a script's timeout covers delivering its input, and a script that writes before it
/// reads can't wedge the server.
#[test]
fn review255_script_timeout_includes_stdin_delivery() -> Result<()> {
    let s = TestServer::start()?;
    for (name, code) in [("noinput", "import time\ntime.sleep(4)\nprint('{}')"), ("chatty", "import sys,time\nsys.stdout.write('x'*1048576)\nsys.stdout.flush()\ntime.sleep(4)")] {
        let r = s.request(Method::POST, "/functions", Some(&json!({"name":name,"kind":"script","runtime":"python","code":code,"timeout_seconds":1,"params":[{"name":"payload","type":"string"}]})), &[])?;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
        let start = std::time::Instant::now();
        let r = s.request(Method::POST, &format!("/functions/{}/run", name), Some(&json!({"params":{"payload":"x".repeat(256*1024)}})), &[])?;
        assert!(start.elapsed() < std::time::Duration::from_secs(3), "A one-second script ran for {:?} (HTTP {}: {})", start.elapsed(), r.status, r.body);
        assert!(r.body.to_string().contains("didn't finish"), "{}", r.body);
    }
    Ok(())
}

/// A script stopped at its timeout takes the processes it started with it (a process group
/// on Unix, a job object on Windows), so nothing it began carries on after the run.
#[test]
fn review255_timed_out_script_children_are_stopped() -> Result<()> {
    let s = TestServer::start()?;
    let temp = tempfile::tempdir()?;
    let marker = temp.path().join("child-survived.txt");
    let code = "import sys,json,subprocess,time\np=json.load(sys.stdin)['params']['path']\nsubprocess.Popen([sys.executable,'-c','import time,sys; time.sleep(4); open(sys.argv[1],\"w\").write(\"x\")',p])\ntime.sleep(30)";
    let r = s.request(Method::POST, "/functions", Some(&json!({"name":"spawner","kind":"script","runtime":"python","code":code,"timeout_seconds":2,"params":[{"name":"path","type":"string","required":true}]})), &[])?;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let r = s.request(Method::POST, "/functions/spawner/run", Some(&json!({"params":{"path":marker}})), &[])?;
    assert!(r.body.to_string().contains("didn't finish"), "{}", r.body);
    std::thread::sleep(std::time::Duration::from_secs(6));
    assert!(!marker.exists(), "A process the timed-out script started kept running and wrote its file");
    Ok(())
}

/// A SQL statement that would hold more documents in memory than limits.sql_max_mb is refused, not run.
#[test]
fn sql_statements_have_a_memory_budget() -> Result<()> {
    let s = TestServer::start_with(hexdb_tests::TestOptions { extra_toml: "\n[limits]\nsql_max_mb = 1\n".into(), ..Default::default() })?;
    let big = "x".repeat(4000);
    let docs: Vec<Value> = (0..400).map(|i| json!({"n": i, "pad": big})).collect();
    let r = s.request(Method::POST, "/notes/_bulk", Some(&json!(docs)), &[])?;
    assert!(r.status.is_success(), "{}", r.body);
    // A join loads both sides in memory: about 3 MB here.
    let r = s.request(Method::POST, "/sql", Some(&json!({"sql":"SELECT a.n FROM notes a JOIN notes b ON a.n = b.n"})), &[])?;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    assert!(r.body.to_string().contains("limits.sql_max_mb"), "{}", r.body);
    Ok(())
}

/// An answer given before the request's body was read (an upload refused at once) still reaches
/// the client, and the client's next request on the same connection works.
#[test]
fn early_answers_to_unread_uploads_reach_the_client() -> Result<()> {
    let s = TestServer::start()?;
    assert!(s.request(Method::POST, "/tessellations", Some(&json!({ "name": "media", "kind": "files" })), &[])?.status.is_success());
    let client = hexdb_tests::http().build()?;
    for size in [600 * 1024, 2 * 1024 * 1024, 40 * 1024 * 1024] {
        // No credentials: refused before a byte of the body is read.
        let refused = client.put(s.url("/media/_files/big.bin")).body(vec![7u8; size]).send()?;
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED, "{} bytes", size);
        // Small enough to drain: the connection stays usable; too big: it's closed, and says so.
        let closes = refused.headers().get("connection").is_some_and(|v| v == "close");
        assert_eq!(closes, size > 32 * 1024 * 1024, "{} bytes", size);
        drop(refused);
        let next = client.get(s.url("/health")).send()?;
        assert!(next.status().is_success(), "the next request after {} bytes", size);
    }
    Ok(())
}
