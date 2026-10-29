//! Field masks: a masked field reads masked everywhere (REST, queries, SQL,
//! GraphQL), can't be written, and can't be inferred. As for hidden fields,
//! each probe runs once aimed at the real value and once at another; the
//! answers must be the same.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::{json, Value};

const CARD: &str = "4111111111111111";
const MASKED: &str = "************1111";

fn answer(server: &TestServer, key: &str, method: Method, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
    let res = server.request_as(Some(key), method, path, body, &[])?;
    let mut body = res.body;
    // SQL echoes the query, and timings differ between runs.
    if let Value::Object(map) = &mut body {
        map.retain(|k, _| !k.ends_with("_ms") && k != "took" && k != "elapsed" && k != "translated");
    }
    Ok((res.status.as_u16(), body))
}

fn same(server: &TestServer, key: &str, what: &str, method: Method, path: &str, real: Value, fake: Value) -> Result<()> {
    let a = answer(server, key, method.clone(), path, Some(&real))?;
    let b = answer(server, key, method, path, Some(&fake))?;
    assert_eq!(a, b, "{} tells the masked value apart:\nreal {} -> {:?}\nfake {} -> {:?}", what, real, a, fake, b);
    Ok(())
}

#[test]
fn masked_fields_read_masked_and_cannot_be_inferred_or_written() -> Result<()> {
    let server = TestServer::start()?;
    let id = server.insert("payments", &json!({ "n": 1, "card": CARD, "holder": { "ssn": "123456789" }, "amount": 25 }))?;
    let role = json!({ "name": "clerk", "permissions": ["read", "write"], "restrictions": { "payments": { "mask": { "card": { "show_last": 4 }, "holder.ssn": {} } } } });
    let res = server.request(Method::POST, "/roles", Some(&role), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    let clerk = server.user_with_roles("cleo", json!([{ "name": "clerk", "tessellations": ["payments"] }]))?;

    // Every way of reading shows the masked value and never the real one.
    let doc = server.request_as(Some(&clerk), Method::GET, &format!("/payments/{}", id), None, &[])?;
    assert_eq!(doc.body["card"], MASKED, "{}", doc.body);
    assert_eq!(doc.body["holder"]["ssn"], "*********");
    assert_eq!(doc.body["amount"], 25);
    for (method, path, body) in [
        (Method::GET, "/payments".to_string(), None),
        (Method::POST, "/payments/_query".to_string(), Some(json!({}))),
        (Method::POST, "/sql".to_string(), Some(json!({ "sql": "SELECT * FROM payments" }))),
        (Method::POST, "/sql".to_string(), Some(json!({ "sql": "SELECT card, holder FROM payments" }))),
        (Method::POST, "/graphql".to_string(), Some(json!({ "query": r#"{ documents(tessellation: "payments") { documents { json field(path: "card") } } }"# }))),
    ] {
        let res = server.request_as(Some(&clerk), method, &path, body.as_ref(), &[])?;
        let text = res.body.to_string();
        assert!(res.status.is_success(), "{} {}", path, text);
        assert!(!text.contains(CARD) && !text.contains("123456789"), "{} shows a masked value: {}", path, text);
        assert!(text.contains(MASKED), "{} doesn't show the masked value: {}", path, text);
    }
    // The administrator still sees it.
    assert_eq!(server.request(Method::GET, &format!("/payments/{}", id), None, &[])?.body["card"], CARD);

    // Filtering, sorting and grouping on it can't tell the real value apart.
    for (real, fake) in [
        (json!({ "card": CARD }), json!({ "card": "4000000000000002" })),
        (json!({ "card": { "$startsWith": "4111" } }), json!({ "card": { "$startsWith": "5555" } })),
        (json!({ "holder.ssn": "123456789" }), json!({ "holder.ssn": "000000000" })),
    ] {
        same(&server, &clerk, "a _query filter", Method::POST, "/payments/_query", json!({ "filter": real }), json!({ "filter": fake }))?;
    }
    let refused = server.request_as(Some(&clerk), Method::POST, "/payments/_query", Some(&json!({ "filter": { "card": CARD } })), &[])?;
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert!(refused.body.to_string().contains("masked"), "{}", refused.body);
    for (real, fake) in [
        (format!("SELECT n FROM payments WHERE card = '{}'", CARD), "SELECT n FROM payments WHERE card = '4000000000000002'".to_string()),
        ("SELECT n FROM payments WHERE card LIKE '4111%'".to_string(), "SELECT n FROM payments WHERE card LIKE '5555%'".to_string()),
    ] {
        same(&server, &clerk, &real, Method::POST, "/sql", json!({ "sql": real }), json!({ "sql": fake }))?;
    }
    let grouped = server.request_as(Some(&clerk), Method::POST, "/payments/_aggregate", Some(&json!({ "group_by": ["card"] })), &[])?;
    assert!(grouped.status.as_u16() >= 400 || !grouped.body.to_string().contains(CARD), "{}", grouped.body);

    // It can't be written, and a full replace keeps the stored value.
    let set = server.request_as(Some(&clerk), Method::PATCH, &format!("/payments/{}", id), Some(&json!({ "card": "5555555555554444" })), &[])?;
    assert_eq!(set.status, 403, "{}", set.body);
    let replaced = server.request_as(Some(&clerk), Method::PUT, &format!("/payments/{}", id), Some(&json!({ "n": 1, "card": MASKED, "amount": 30 })), &[])?;
    assert!(replaced.status.is_success(), "{}", replaced.body);
    let stored = server.request(Method::GET, &format!("/payments/{}", id), None, &[])?.body;
    assert_eq!(stored["card"], CARD, "{}", stored);
    assert_eq!(stored["amount"], 30);

    // Masks are checked when a role is saved.
    for bad in [json!({ "card": { "show_last": 5 } }), json!({ "card": { "show_first": 7 } }), json!({ "id": {} })] {
        let role = json!({ "name": "bad", "permissions": ["read"], "restrictions": { "payments": { "mask": bad } } });
        let res = server.request(Method::POST, "/roles", Some(&role), &[])?;
        assert_eq!(res.status, 400, "{} -> {}", role, res.body);
    }
    Ok(())
}

#[test]
fn status_reports_the_sealed_fields() -> Result<()> {
    let server = TestServer::start()?;
    let role = json!({ "name": "clerk", "permissions": ["read"], "restrictions": { "payments": { "mask": { "card": {} } }, "*": { "mask": { "ssn": {} } } } });
    assert!(server.request(Method::POST, "/roles", Some(&role), &[])?.status.is_success());
    let status = server.request(Method::GET, "/status", None, &[])?.body;
    assert_eq!(status["memory_protection"]["sealed_fields"], json!({ "payments": ["card"], "*": ["ssn"] }), "{}", status["memory_protection"]);
    if cfg!(unix) {
        assert_eq!(status["memory_protection"]["core_dumps"], false, "core dumps are off by default");
    }
    Ok(())
}
