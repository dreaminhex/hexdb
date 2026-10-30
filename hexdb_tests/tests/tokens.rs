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

#[test]
fn vault_entries_are_per_tessellation_and_unused_ones_are_deleted() -> Result<()> {
    let server = TestServer::start()?;
    let schema = json!({ "fields": { "card": { "type": "string", "tokenize": "vault" } } });
    for tess in ["accounts", "payments"] {
        server.insert(tess, &json!({ "card": "0000000000000000" }))?;
        let res = server.request(Method::POST, &format!("/tessellations/{}/schemas", tess), Some(&schema), &[])?;
        assert!(res.status.is_success(), "{}", res.body);
    }
    let detokenize = |tess: &str, token: &str| -> Result<Value> {
        let res = server.request(Method::POST, &format!("/tessellations/{}/detokenize", tess), Some(&json!({ "tokens": [token] })), &[])?;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        Ok(res.body["values"][token].clone())
    };
    let compact = || -> Result<u64> {
        let res = server.request(Method::POST, "/compact", None, &[])?;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        Ok(res.body["vault_tokens_deleted"].as_u64().unwrap())
    };

    // The same card in two tessellations: one token, an entry in each.
    let account = server.insert("accounts", &json!({ "card": CARD }))?;
    let payment = server.insert("payments", &json!({ "card": CARD }))?;
    let token = server.get_doc("accounts", &account)?.unwrap()["card"].as_str().unwrap().to_string();
    assert_eq!(server.get_doc("payments", &payment)?.unwrap()["card"], token.as_str());
    assert_eq!(detokenize("accounts", &token)?, CARD);
    assert_eq!(detokenize("payments", &token)?, CARD);
    assert_eq!(compact()?, 0, "every entry is in use");

    // Deleting the account leaves its entry unused; the payment's stays.
    let res = server.request(Method::DELETE, &format!("/accounts/{}", account), None, &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    assert_eq!(compact()?, 1);
    assert!(detokenize("accounts", &token)?.is_null());
    assert_eq!(detokenize("payments", &token)?, CARD);

    // A changed value leaves the old token's entry unused.
    let res = server.request(Method::PATCH, &format!("/payments/{}", payment), Some(&json!({ "card": "5500005555555559" })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    let new_token = server.get_doc("payments", &payment)?.unwrap()["card"].as_str().unwrap().to_string();
    assert_eq!(compact()?, 1);
    assert!(detokenize("payments", &token)?.is_null());
    assert_eq!(detokenize("payments", &new_token)?, "5500005555555559");
    Ok(())
}

#[test]
fn sql_beyond_one_table_compares_tokenized_fields_by_value() -> Result<()> {
    let server = TestServer::start()?;
    let schema = json!({ "fields": { "card": { "type": "string", "tokenize": "one_way" } } });
    for tess in ["accounts", "payments"] {
        server.insert(tess, &json!({ "card": "0000000000000000" }))?;
        let res = server.request(Method::POST, &format!("/tessellations/{}/schemas", tess), Some(&schema), &[])?;
        assert!(res.status.is_success(), "{}", res.body);
    }
    server.insert("accounts", &json!({ "name": "Ada", "card": CARD }))?;
    server.insert("accounts", &json!({ "name": "Bo", "card": "5500005555555559" }))?;
    server.insert("payments", &json!({ "amount": 12, "card": CARD }))?;
    let sql = |statement: &str, params: Value| -> Result<Value> {
        let res = server.request(Method::POST, "/sql", Some(&json!({ "sql": statement, "params": params })), &[])?;
        assert_eq!(res.status, StatusCode::OK, "{}: {}", statement, res.body);
        Ok(res.body["rows"].clone())
    };
    // A join on the tokenized field, filtered by the real value.
    assert_eq!(
        sql("SELECT a.name, p.amount FROM accounts a JOIN payments p ON p.card = a.card WHERE a.card = ?", json!([CARD]))?,
        json!([["Ada", 12]])
    );
    // Expressions run in memory: a function, IN, <> and a subquery.
    assert_eq!(sql("SELECT UPPER(name) FROM accounts WHERE card IN (?, 'nope')", json!([CARD]))?, json!([["ADA"]]));
    assert_eq!(sql("SELECT UPPER(name) FROM accounts WHERE card <> ? AND name IS NOT NULL", json!([CARD]))?, json!([["BO"]]));
    assert_eq!(
        sql("SELECT name FROM accounts WHERE card IN (SELECT card FROM payments WHERE card = ?)", json!([CARD]))?,
        json!([["Ada"]])
    );
    Ok(())
}

#[test]
fn answers_and_idempotency_records_hold_tokens_and_rules_accept_them() -> Result<()> {
    let server = TestServer::start()?;
    server.insert("accounts", &json!({ "name": "seed" }))?;
    // max_length 16 fits the card, not its 35-character token: rules check the value, once.
    let schema = json!({ "fields": { "card": { "type": "string", "tokenize": "vault", "max_length": 16 } } });
    let res = server.request(Method::POST, "/tessellations/accounts/schemas", Some(&schema), &[])?;
    assert!(res.status.is_success(), "{}", res.body);

    // The insert's answer, and its replay under the same Idempotency-Key, show the token.
    let key = [("Idempotency-Key", "card-1")];
    let first = server.request(Method::POST, "/accounts", Some(&json!({ "name": "Ada", "card": CARD })), &key)?;
    assert!(first.status.is_success(), "{}", first.body);
    let token = first.body["card"].as_str().unwrap().to_string();
    assert!(token.starts_with("tok_"), "{}", first.body);
    let replay = server.request(Method::POST, "/accounts", Some(&json!({ "name": "Ada", "card": CARD })), &key)?;
    assert_eq!(replay.body["card"], token.as_str(), "{}", replay.body);
    let id = first.body["id"].as_str().unwrap().to_string();

    // Patching another field, and writing the document back, keep the token within max_length.
    let res = server.request(Method::PATCH, &format!("/accounts/{}", id), Some(&json!({ "name": "Ada L." })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    assert_eq!(res.body["card"], token.as_str());
    let mut doc = server.get_doc("accounts", &id)?.unwrap();
    doc["name"] = json!("Ada Lovelace");
    let res = server.request(Method::PUT, &format!("/accounts/{}", id), Some(&doc), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    let long = server.request(Method::POST, "/accounts", Some(&json!({ "card": "41111111111111112222" })), &[])?;
    assert_eq!(long.status.as_u16(), 422, "the value itself is still checked: {}", long.body);

    // A transaction's results show tokens too.
    let tx = json!({ "operations": [{ "op": "insert", "tessellation": "accounts", "data": { "name": "Bo", "card": "5500005555555559" } }] });
    let res = server.request(Method::POST, "/transactions", Some(&tx), &[("Idempotency-Key", "tx-1")])?;
    assert!(res.status.is_success(), "{}", res.body);
    assert!(res.body["results"][0]["document"]["card"].as_str().unwrap().starts_with("tok_"), "{}", res.body);
    assert!(!res.body.to_string().contains("5500005555555559"));
    Ok(())
}
