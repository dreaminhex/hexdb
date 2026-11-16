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
