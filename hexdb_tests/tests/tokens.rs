//! Tokenization: fields a schema marks `tokenize` hold tokens, never values;
//! equality lookups still work; vault tokens can be detokenized with the
//! `detokenize` permission (audited), one-way tokens never.

use anyhow::{bail, Result};
use hexdb_tests::TestServer;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const CARD: &str = "4111111111111111";
const SSN: &str = "123-45-6789";

fn query(server: &TestServer, filter: Value) -> Result<Vec<Value>> {
    let res = server.request(Method::POST, "/accounts/_query", Some(&json!({ "filter": filter })), &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    Ok(res.body["documents"].as_array().cloned().unwrap_or_default())
}

#[test]
fn tokenized_fields_hold_tokens_and_vault_tokens_detokenize() -> Result<()> {
    let server = TestServer::start()?;
    // A document written before the schema: the schema's migration tokenizes it.
    let early = server.insert("accounts", &json!({ "name": "Early", "card": "5500005555555559", "ssn": "987-65-4321" }))?;
    let schema = json!({ "fields": {
        "name": { "type": "string" },
        "card": { "type": "string", "tokenize": "vault", "min_length": 12 },
        "ssn": { "type": "string", "tokenize": "one_way" }
    } });
    let res = server.request(Method::POST, "/tessellations/accounts/schemas", Some(&schema), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    server.insert("other", &json!({ "n": 1 }))?;
    let bad = server.request(Method::POST, "/tessellations/other/schemas", Some(&json!({ "fields": { "n": { "type": "number", "tokenize": "vault" } } })), &[])?;
    assert_eq!(bad.status, StatusCode::CONFLICT, "only strings are tokenized: {}", bad.body);
    assert!(bad.body.to_string().contains("only string fields"), "{}", bad.body);

    let id = server.insert("accounts", &json!({ "name": "Ada", "card": CARD, "ssn": SSN }))?;
    let doc = server.get_doc("accounts", &id)?.unwrap();
    let (card, ssn) = (doc["card"].as_str().unwrap().to_string(), doc["ssn"].as_str().unwrap().to_string());
    assert!(card.starts_with("tok_") && card.ends_with("_1111"), "{}", card);
    assert!(ssn.starts_with("tok_") && ssn.ends_with("_6789"), "{}", ssn);
    assert!(!doc.to_string().contains(CARD) && !doc.to_string().contains(SSN));
    // Rules check the real value before it's replaced.
    let short = server.request(Method::POST, "/accounts", Some(&json!({ "name": "Bo", "card": "4111" })), &[])?;
    assert_eq!(short.status.as_u16(), 422, "{}", short.body);

    // Lookups by the real value find the document (REST and SQL); the token works too.
    assert_eq!(query(&server, json!({ "card": CARD }))?.len(), 1);
    assert_eq!(query(&server, json!({ "ssn": { "$in": [SSN, "000-00-0000"] } }))?.len(), 1);
    assert_eq!(query(&server, json!({ "card": card }))?.len(), 1);
    let sql = server.request(Method::POST, "/sql", Some(&json!({ "sql": "SELECT name FROM accounts WHERE card = ?", "params": [CARD] })), &[])?;
    assert_eq!(sql.body["rows"], json!([["Ada"]]), "{}", sql.body);

    // Writing a document back as it was read keeps its tokens as they are.
    let mut edited = doc.clone();
    edited["name"] = json!("Ada L.");
    let res = server.request(Method::PUT, &format!("/accounts/{}", id), Some(&edited), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    assert_eq!(server.get_doc("accounts", &id)?.unwrap()["card"], card.as_str());

    // The earlier document is migrated to tokens.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let early_doc = server.get_doc("accounts", &early)?.unwrap();
        if early_doc["card"].as_str().is_some_and(|c| c.starts_with("tok_")) {
            assert!(early_doc["ssn"].as_str().unwrap().starts_with("tok_"));
            break;
        }
        if Instant::now() > deadline {
            bail!("the earlier document was never tokenized: {}", early_doc);
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Vault tokens detokenize; one-way tokens never do.
    let detokenize = |key: Option<&str>, tokens: Value| -> Result<hexdb_tests::ApiResponse> {
        let body = json!({ "tokens": tokens });
        match key {
            Some(key) => server.request_as(Some(key), Method::POST, "/tessellations/accounts/detokenize", Some(&body), &[]),
            None => server.request(Method::POST, "/tessellations/accounts/detokenize", Some(&body), &[]),
        }
    };
    let res = detokenize(None, json!([card, ssn, "tok_nothing"]))?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["values"][&card], CARD);
    assert!(res.body["values"][&ssn].is_null(), "one-way tokens can't be reversed");

    // Permissions: read alone isn't enough; detokenize is; a role that masks the card isn't.
    let reader = server.user_with_roles("rita", json!([{ "name": "reader", "tessellations": ["accounts"] }]))?;
    assert_eq!(detokenize(Some(&reader), json!([card]))?.status, StatusCode::FORBIDDEN);
    for role in [
        json!({ "name": "vault", "permissions": ["read", "detokenize"] }),
        json!({ "name": "masked_vault", "permissions": ["read", "detokenize"], "restrictions": { "accounts": { "mask": { "card": { "show_last": 4 } } } } }),
    ] {
        assert!(server.request(Method::POST, "/roles", Some(&role), &[])?.status.is_success());
    }
    let clerk = server.user_with_roles("vic", json!([{ "name": "vault", "tessellations": ["accounts"] }]))?;
    assert_eq!(detokenize(Some(&clerk), json!([card]))?.body["values"][&card], CARD);
    let masked = server.user_with_roles("max", json!([{ "name": "masked_vault", "tessellations": ["accounts"] }]))?;
    assert_eq!(detokenize(Some(&masked), json!([card]))?.status, StatusCode::FORBIDDEN);

    let audit = server.request(Method::GET, "/audit?action=token.detokenize", None, &[])?;
    let events = audit.body["events"].as_array().cloned().unwrap_or_default();
    assert!(events.len() >= 2, "{}", audit.body);
    assert!(!audit.body.to_string().contains(CARD), "values never reach the audit trail");
    Ok(())
}
