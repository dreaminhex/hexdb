//! Cases the 2026-11-10 code review asked for beyond its own regression tests
//! (tests/review_regressions.rs), checked over the API.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};

fn tessellation_with_schema(server: &TestServer, fields: Value) -> Result<()> {
    let r = server.request(Method::POST, "/tessellations", Some(&json!({ "name": "accounts" })), &[])?;
    assert!(r.status.is_success(), "{}", r.body);
    let r = server.request(Method::POST, "/tessellations/accounts/schemas", Some(&json!({ "fields": fields, "additional_fields": true })), &[])?;
    assert!(r.status.is_success(), "{}", r.body);
    Ok(())
}

fn user(server: &TestServer, name: &str, permissions: Value, restriction: Value) -> Result<String> {
    let r = server.request(Method::POST, "/roles", Some(&json!({ "name": name, "permissions": permissions, "restrictions": { "accounts": restriction } })), &[])?;
    assert!(r.status.is_success(), "{}", r.body);
    server.user_with_roles(name, json!([{ "name": name, "tessellations": ["accounts"] }]))
}

fn detokenize(server: &TestServer, key: &str, token: &Value) -> Result<(StatusCode, Value)> {
    let r = server.request_as(Some(key), Method::POST, "/tessellations/accounts/detokenize", Some(&json!({ "tokens": [token] })), &[])?;
    Ok((r.status, r.body))
}

/// HDB-01: a tenant still opens its own tokens; a masked parent and another
/// tenant's token reveal nothing.
#[test]
fn detokenize_follows_the_tokens_document_and_field() -> Result<()> {
    let server = TestServer::start()?;
    tessellation_with_schema(&server, json!({ "card": { "type": "string", "tokenize": "vault" }, "profile.pin": { "type": "string", "tokenize": "vault" } }))?;
    let a = server.insert("accounts", &json!({ "tenant": "A", "card": "4111111111111111", "profile": { "pin": "1234" } }))?;
    let b = server.insert("accounts", &json!({ "tenant": "B", "card": "5500005555555559" }))?;
    let token_a = server.get_doc("accounts", &a)?.unwrap()["card"].clone();
    let token_b = server.get_doc("accounts", &b)?.unwrap()["card"].clone();
    let pin_a = server.get_doc("accounts", &a)?.unwrap()["profile"]["pin"].clone();

    let tenant = user(&server, "tenant-a", json!(["read", "detokenize"]), json!({ "filter": { "tenant": "A" } }))?;
    let (status, body) = detokenize(&server, &tenant, &token_a)?;
    assert_eq!((status, body["values"][token_a.as_str().unwrap()].as_str()), (StatusCode::OK, Some("4111111111111111")), "{}", body);
    let (_, body) = detokenize(&server, &tenant, &token_b)?;
    assert!(body["values"][token_b.as_str().unwrap()].is_null(), "{}", body);

    // A mask on a field around the tokenized one counts too.
    let masked = user(&server, "masked-parent", json!(["read", "detokenize"]), json!({ "mask": { "profile": { "show_last": 0 } } }))?;
    let (status, body) = detokenize(&server, &masked, &pin_a)?;
    assert!(status == StatusCode::FORBIDDEN || body["values"][pin_a.as_str().unwrap()].is_null(), "{} {}", status, body);
    assert!(!body.to_string().contains("1234"), "{}", body);
    Ok(())
}

/// HDB-02: tokenized and hidden fields inside arrays are protected element by element.
#[test]
fn fields_inside_arrays_are_tokenized_and_hidden() -> Result<()> {
    let server = TestServer::start()?;
    tessellation_with_schema(&server, json!({ "contacts.card": { "type": "string", "tokenize": "vault" } }))?;
    let id = server.insert("accounts", &json!({ "contacts": [{ "card": "4111111111111111", "ssn": "111" }, { "card": "5500005555555559", "ssn": "222" }] }))?;
    let stored = server.get_doc("accounts", &id)?.unwrap();
    for contact in stored["contacts"].as_array().unwrap() {
        assert!(contact["card"].as_str().unwrap().starts_with("tok_"), "each element's value is a token: {}", stored);
    }
    let reader = user(&server, "no-ssn", json!(["read"]), json!({ "hide": ["contacts.ssn"] }))?;
    let seen = server.request_as(Some(&reader), Method::GET, &format!("/accounts/{}", id), None, &[])?.body;
    assert!(!seen.to_string().contains("\"ssn\""), "{}", seen);
    let found = server.request_as(Some(&reader), Method::POST, "/accounts/_query", Some(&json!({ "filter": {} })), &[])?.body;
    assert!(!found.to_string().contains("\"ssn\""), "queries too: {}", found);
    Ok(())
}

/// HDB-03: two roles each hiding the SSN (one through its parent) never show it together.
#[test]
fn combined_roles_never_show_what_both_hide() -> Result<()> {
    let server = TestServer::start()?;
    server.insert("accounts", &json!({ "profile": { "ssn": "synthetic-secret", "name": "Ada" } }))?;
    for (name, hide) in [("hide-profile", json!(["profile"])), ("hide-ssn", json!(["profile.ssn"]))] {
        let r = server.request(Method::POST, "/roles", Some(&json!({ "name": name, "permissions": ["read"], "restrictions": { "accounts": { "hide": hide } } })), &[])?;
        assert!(r.status.is_success(), "{}", r.body);
    }
    let both = server.user_with_roles("both", json!([{ "name": "hide-profile", "tessellations": ["accounts"] }, { "name": "hide-ssn", "tessellations": ["accounts"] }]))?;
    let found = server.request_as(Some(&both), Method::POST, "/accounts/_query", Some(&json!({ "filter": {} })), &[])?.body;
    assert!(!found.to_string().contains("synthetic-secret"), "{}", found);
    assert!(found.to_string().contains("Ada"), "the name is visible through the second role: {}", found);
    let sql = server.request_as(Some(&both), Method::POST, "/sql", Some(&json!({ "query": "SELECT * FROM accounts" })), &[])?.body;
    assert!(!sql.to_string().contains("synthetic-secret"), "{}", sql);
    Ok(())
}

/// HDB-05: a replay is held to the caller's rows as they are now: once the
/// row is outside them, the replay is refused and nothing is written again.
#[test]
fn a_replay_after_row_access_is_revoked_is_refused() -> Result<()> {
    let server = TestServer::start()?;
    server.insert("accounts", &json!({ "tenant": "seed" }))?;
    let writer = user(&server, "tenant-writer", json!(["read", "write"]), json!({ "filter": { "tenant": "A" } }))?;
    let body = json!({ "tenant": "A", "n": 1 });
    let key = [("Idempotency-Key", "review-row-revoked")];
    let first = server.request_as(Some(&writer), Method::POST, "/accounts", Some(&body), &key)?;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    // The role now covers tenant B only.
    let r = server.request(Method::PUT, "/roles/tenant-writer", Some(&json!({ "name": "tenant-writer", "permissions": ["read", "write"], "restrictions": { "accounts": { "filter": { "tenant": "B" } } } })), &[])?;
    assert!(r.status.is_success(), "{}", r.body);
    let replay = server.request_as(Some(&writer), Method::POST, "/accounts", Some(&body), &key)?;
    assert_eq!(replay.status, StatusCode::FORBIDDEN, "{}", replay.body);
    let all = server.request(Method::POST, "/accounts/_query", Some(&json!({ "filter": { "tenant": "A" } })), &[])?.body;
    assert_eq!(all["documents"].as_array().map(Vec::len), Some(1), "written once: {}", all);
    Ok(())
}
