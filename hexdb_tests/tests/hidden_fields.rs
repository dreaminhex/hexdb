//! Hidden fields and row filters can't be inferred. Each probe runs twice: once
//! aimed at a value the hidden field (or a filtered-out row) really has, once at
//! one it doesn't. The two answers must be the same (both refused is fine):
//! any difference would tell the caller something it can't read.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::{json, Value};

/// Drop timings, which differ from run to run, and SQL's echo of the caller's own query.
fn stable(mut v: Value) -> Value {
    match &mut v {
        Value::Object(map) => {
            map.retain(|k, _| !k.ends_with("_ms") && k != "took" && k != "elapsed" && k != "translated");
            for value in map.values_mut() {
                *value = stable(value.take());
            }
        }
        Value::Array(list) => {
            for value in list.iter_mut() {
                *value = stable(value.take());
            }
        }
        _ => {}
    }
    v
}

fn answer(server: &TestServer, key: &str, method: Method, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
    let res = server.request_as(Some(key), method, path, body, &[])?;
    Ok((res.status.as_u16(), stable(res.body)))
}

fn same(server: &TestServer, key: &str, what: &str, method: Method, path: &str, real: Value, fake: Value) -> Result<()> {
    let a = answer(server, key, method.clone(), path, Some(&real))?;
    let b = answer(server, key, method, path, Some(&fake))?;
    assert_eq!(a, b, "{} tells the hidden value apart:\nreal {} -> {:?}\nfake {} -> {:?}", what, real, a, fake, b);
    Ok(())
}

/// Refused, or answered without any hidden value or filtered-out row.
fn blind(server: &TestServer, key: &str, what: &str, method: Method, path: &str, body: Value) -> Result<()> {
    let (status, answer) = answer(server, key, method, path, Some(&body))?;
    let text = answer.to_string();
    assert!(status >= 400 || !["alpha", "omega", "usonly"].iter().any(|v| text.contains(v)), "{} shows a hidden value: {}", what, text);
    Ok(())
}

fn sql(query: &str) -> Value {
    json!({ "sql": query })
}

fn gql(query: &str) -> Value {
    json!({ "query": query })
}

#[test]
fn hidden_fields_and_filtered_rows_cannot_be_inferred() -> Result<()> {
    let server = TestServer::start()?;
    for (n, region, secret) in [(1, "eu", "alpha alphaword"), (2, "eu", "omega"), (3, "us", "usonly")] {
        server.insert("notes", &json!({ "n": n, "region": region, "secret": secret, "nested": { "secret": secret } }))?;
    }
    let restrictions = json!({ "notes": { "filter": { "region": "eu" }, "hide": ["secret", "nested.secret"] } });
    for (name, permissions) in [("eu_reader", json!(["read"])), ("eu_writer", json!(["read", "write"]))] {
        let role = json!({ "name": name, "permissions": permissions, "restrictions": restrictions });
        assert!(server.request(Method::POST, "/roles", Some(&role), &[])?.status.is_success());
    }
    let reader = server.user_with_roles("rita", json!([{ "name": "eu_reader", "tessellations": ["notes"] }]))?;
    let writer = server.user_with_roles("walt", json!([{ "name": "eu_writer", "tessellations": ["notes"] }]))?;
    let q = "/notes/_query";

    // The oracle works: for the admin, who can see the field, the answers differ.
    let admin = server.token().to_string();
    let seen = |value: &str| answer(&server, &admin, Method::POST, "/notes/_query", Some(&json!({ "filter": { "secret": value } })));
    assert_ne!(seen("omega")?, seen("nope")?);

    // Filters on a hidden field, however they're written.
    for (real, fake) in [
        (json!({ "secret": "omega" }), json!({ "secret": "nope" })),
        (json!({ "nested.secret": "omega" }), json!({ "nested.secret": "nope" })),
        (json!({ "nested": { "secret": "omega" } }), json!({ "nested": { "secret": "nope" } })),
        (json!({ "$or": [{ "secret": "omega" }, { "n": 99 }] }), json!({ "$or": [{ "secret": "nope" }, { "n": 99 }] })),
        (json!({ "$not": { "secret": "omega" } }), json!({ "$not": { "secret": "nope" } })),
        (json!({ "secret": { "$gt": "m" } }), json!({ "secret": { "$gt": "zzz" } })),
        (json!({ "secret": { "$startsWith": "om" } }), json!({ "secret": { "$startsWith": "qq" } })),
        (json!({ "secret": { "$exists": true } }), json!({ "secret": { "$exists": false } })),
        (json!({ "_or": [{ "secret": "omega" }] }), json!({ "_or": [{ "secret": "nope" }] })),
        (json!({ "$text": "alphaword" }), json!({ "$text": "nonexistentword" })),
        (json!({ "$text": "usonly" }), json!({ "$text": "nonexistentword" })),
    ] {
        same(&server, &reader, "a _query filter", Method::POST, q, json!({ "filter": real }), json!({ "filter": fake }))?;
    }
    // Sorting by a hidden field.
    same(&server, &reader, "sorting", Method::POST, q, json!({ "sort": [{ "field": "secret" }] }), json!({ "sort": [{ "field": "secret", "descending": true }] }))?;

    // Aggregations.
    blind(&server, &reader, "grouping", Method::POST, "/notes/_aggregate", json!({ "group_by": ["secret"] }))?;
    blind(&server, &reader, "an aggregate", Method::POST, "/notes/_aggregate", json!({ "aggregates": { "m": { "$max": "secret" } } }))?;
    blind(&server, &reader, "a nested aggregate", Method::POST, "/notes/_aggregate", json!({ "group_by": ["nested.secret"] }))?;
    same(
        &server,
        &reader,
        "an aggregate's filter",
        Method::POST,
        "/notes/_aggregate",
        json!({ "filter": { "secret": "omega" }, "aggregates": { "c": { "$count": {} } } }),
        json!({ "filter": { "secret": "nope" }, "aggregates": { "c": { "$count": {} } } }),
    )?;

    // SQL, clause by clause.
    for (real, fake) in [
        ("SELECT n FROM notes WHERE secret = 'omega'", "SELECT n FROM notes WHERE secret = 'nope'"),
        ("SELECT n FROM notes WHERE secret LIKE 'om%'", "SELECT n FROM notes WHERE secret LIKE 'qq%'"),
        ("SELECT n FROM notes WHERE nested.secret = 'omega'", "SELECT n FROM notes WHERE nested.secret = 'nope'"),
        ("SELECT n FROM notes ORDER BY secret", "SELECT n FROM notes ORDER BY secret DESC"),
        ("SELECT region, COUNT(*) FROM notes GROUP BY region HAVING MAX(secret) > 'n'", "SELECT region, COUNT(*) FROM notes GROUP BY region HAVING MAX(secret) > 'zzz'"),
        ("SELECT n FROM notes WHERE region = 'us'", "SELECT n FROM notes WHERE region = 'xx'"),
    ] {
        same(&server, &reader, real, Method::POST, "/sql", sql(real), sql(fake))?;
    }
    for query in ["SELECT secret, COUNT(*) FROM notes GROUP BY secret", "SELECT MAX(secret) FROM notes", "SELECT MIN(nested.secret) FROM notes", "SELECT COUNT(secret) FROM notes"] {
        blind(&server, &reader, query, Method::POST, "/sql", sql(query))?;
    }
    let all = server.request_as(Some(&reader), Method::POST, "/sql", Some(&sql("SELECT * FROM notes")), &[])?;
    assert!(!all.body.to_string().contains("omega") && !all.body.to_string().contains("usonly"), "{}", all.body);

    // GraphQL.
    for (real, fake) in [
        (r#"{ documents(tessellation: "notes", filter: { secret: "omega" }) { total } }"#, r#"{ documents(tessellation: "notes", filter: { secret: "nope" }) { total } }"#),
        (r#"{ count(tessellation: "notes", filter: { secret: { _gt: "m" } }) }"#, r#"{ count(tessellation: "notes", filter: { secret: { _gt: "zzz" } }) }"#),
        (
            r#"{ documents(tessellation: "notes", sort: [{ field: "secret" }]) { documents { field(path: "n") } } }"#,
            r#"{ documents(tessellation: "notes", sort: [{ field: "secret", descending: true }]) { documents { field(path: "n") } } }"#,
        ),
    ] {
        same(&server, &reader, real, Method::POST, "/graphql", gql(real), gql(fake))?;
    }

    blind(&server, &reader, "GraphQL field()", Method::POST, "/graphql", gql(r#"{ documents(tessellation: "notes") { documents { field(path: "secret") json data } } }"#))?;
    blind(&server, &reader, "GraphQL aggregate", Method::POST, "/graphql", gql(r#"{ aggregate(tessellation: "notes", groupBy: ["secret"]) }"#))?;

    // Counting and listing with a filter in the query string.
    let count = |filter: &str| answer(&server, &reader, Method::GET, &format!("/notes/count?filter={}", urlencode(filter)), None);
    assert_eq!(count(r#"{"secret":"omega"}"#)?, count(r#"{"secret":"nope"}"#)?, "count");
    let list = |filter: &str| answer(&server, &reader, Method::GET, &format!("/notes?filter={}&sort=secret", urlencode(filter)), None);
    assert_eq!(list(r#"{"secret":"omega"}"#)?, list(r#"{"secret":"nope"}"#)?, "listing");

    // A write filtered on a hidden field (the matched count would tell).
    same(
        &server,
        &writer,
        "_update",
        Method::POST,
        "/notes/_update",
        json!({ "filter": { "secret": "omega" }, "update": { "touched": true } }),
        json!({ "filter": { "secret": "nope" }, "update": { "touched": true } }),
    )?;

    // Rows outside the row filter don't show in counts or query plans.
    let before = answer(&server, &reader, Method::POST, q, Some(&json!({})))?;
    for n in 10..20 {
        server.insert("notes", &json!({ "n": n, "region": "us", "secret": "more" }))?;
    }
    let after = answer(&server, &reader, Method::POST, q, Some(&json!({})))?;
    assert_eq!(before, after, "adding rows the reader can't see changed what it sees");
    Ok(())
}

fn urlencode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{:02X}", b) }).collect()
}

#[test]
fn transactions_and_ids_dont_reveal_hidden_rows_or_fields() -> Result<()> {
    let server = TestServer::start()?;
    let eu = server.insert("notes", &json!({ "n": 1, "region": "eu", "secret": "omega" }))?;
    let us = server.insert("notes", &json!({ "n": 2, "region": "us", "secret": "usonly" }))?;
    let nowhere = ulid::Ulid::new().to_string();
    let role = json!({ "name": "eu_writer", "permissions": ["read", "write"], "restrictions": { "notes": { "filter": { "region": "eu" }, "hide": ["secret"] } } });
    assert!(server.request(Method::POST, "/roles", Some(&role), &[])?.status.is_success());
    let writer = server.user_with_roles("walt", json!([{ "name": "eu_writer", "tessellations": ["notes"] }]))?;
    let tx = |op: Value| json!({ "operations": [op] });

    // A precondition on a hidden field.
    same(
        &server,
        &writer,
        "if_match on a hidden field",
        Method::POST,
        "/transactions",
        tx(json!({ "op": "check", "tessellation": "notes", "id": eu, "if_match": { "secret": "omega" } })),
        tx(json!({ "op": "check", "tessellation": "notes", "id": eu, "if_match": { "secret": "nope" } })),
    )?;

    // A row outside the filter looks like one that doesn't exist, whatever is asked of it.
    for (what, op) in [
        ("get", json!({ "op": "get", "tessellation": "notes" })),
        ("check", json!({ "op": "check", "tessellation": "notes" })),
        ("if_version 0", json!({ "op": "check", "tessellation": "notes", "if_version": 0 })),
        ("if_match {}", json!({ "op": "check", "tessellation": "notes", "if_match": {} })),
        ("patch", json!({ "op": "patch", "tessellation": "notes", "data": { "n": 9 } })),
        ("delete", json!({ "op": "delete", "tessellation": "notes" })),
    ] {
        let with = |id: &str| {
            let mut op = op.clone();
            op["id"] = json!(id);
            tx(op)
        };
        // Error messages name the ID that was asked for.
        let a = answer(&server, &writer, Method::POST, "/transactions", Some(&with(&us)))?;
        let b = answer(&server, &writer, Method::POST, "/transactions", Some(&with(&nowhere)))?;
        let unnamed = |(status, body): (u16, Value), id: &str| (status, body.to_string().replace(id, "<id>"));
        assert_eq!(unnamed(a, &us), unnamed(b, &nowhere), "{} tells a filtered-out row from a missing one", what);
    }
    for method in [Method::GET, Method::DELETE] {
        let a = answer(&server, &writer, method.clone(), &format!("/notes/{}", us), None)?;
        let b = answer(&server, &writer, method.clone(), &format!("/notes/{}", nowhere), None)?;
        assert_eq!(a.0, b.0, "{} of a filtered-out row: {:?} vs {:?}", method, a, b);
    }
    let patched = answer(&server, &writer, Method::PATCH, &format!("/notes/{}", us), Some(&json!({ "n": 9 })))?;
    let missing = answer(&server, &writer, Method::PATCH, &format!("/notes/{}", nowhere), Some(&json!({ "n": 9 })))?;
    assert_eq!(patched.0, missing.0, "PATCH of a filtered-out row: {:?} vs {:?}", patched, missing);
    let doc = server.get_doc("notes", &us)?.expect("the US note is untouched");
    assert_eq!(doc["n"], 2, "{}", doc);

    // What a transaction returns is masked.
    let res = server.request_as(Some(&writer), Method::POST, "/transactions", Some(&tx(json!({ "op": "get", "tessellation": "notes", "id": eu }))), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    assert!(!res.body.to_string().contains("omega"), "{}", res.body);
    let res = server.request_as(Some(&writer), Method::POST, "/transactions", Some(&tx(json!({ "op": "patch", "tessellation": "notes", "id": eu, "data": { "n": 5 } }))), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    assert!(!res.body.to_string().contains("omega"), "{}", res.body);

    // Writes: REST and transactions agree on rows outside the filter, and other tessellations need a grant.
    let rest = server.request_as(Some(&writer), Method::POST, "/notes", Some(&json!({ "region": "us", "n": 7 })), &[])?.status;
    let txn = server.request_as(Some(&writer), Method::POST, "/transactions", Some(&tx(json!({ "op": "insert", "tessellation": "notes", "data": { "region": "us", "n": 7 } }))), &[])?.status;
    assert_eq!(rest.is_success(), txn.is_success(), "REST {} vs transaction {}", rest, txn);
    let other = server.request_as(Some(&writer), Method::POST, "/transactions", Some(&tx(json!({ "op": "insert", "tessellation": "payroll", "data": { "x": 1 } }))), &[])?.status;
    assert_eq!(other.as_u16(), 403);
    Ok(())
}
