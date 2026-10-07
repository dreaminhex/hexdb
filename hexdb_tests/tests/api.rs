//! API contract tests: response shapes and status codes, tessellations, bulk
//! writes, update by filter, idempotency keys, users and roles.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::thread;

fn ids_of(body: &Value) -> Vec<String> {
    body["ids"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

#[test]
fn insert_returns_201_with_plain_document() -> Result<()> {
    let server = TestServer::start()?;
    let res = server.request(Method::POST, "/articles?ttl=3600", Some(&json!({ "title": "Hello", "tags": ["a", "b"], "id": "ignored" })), &[])?;

    assert_eq!(res.status, StatusCode::CREATED);
    let id = res.body["id"].as_str().unwrap().to_string();
    assert_ne!(id, "ignored", "client-supplied ids are ignored");
    assert_eq!(res.headers["location"], format!("/articles/{}", id).as_str());
    assert_eq!(res.body["title"], "Hello");
    assert_eq!(res.body["tags"], json!(["a", "b"]));
    assert!(res.body["_expires_at"].is_string());

    let fetched = server.request(Method::GET, &format!("/articles/{}", id), None, &[])?;
    assert_eq!(fetched.status, StatusCode::OK);
    assert_eq!(fetched.body, res.body);
    Ok(())
}

#[test]
fn missing_documents_return_404_with_error_body() -> Result<()> {
    let server = TestServer::start()?;
    server.insert("articles", &json!({ "title": "x" }))?;
    let missing = "01JTY87RVJ9B5863KMB2YD896B";

    for (method, body) in [
        (Method::GET, None),
        (Method::PUT, Some(json!({ "title": "y" }))),
        (Method::PATCH, Some(json!({ "title": "y" }))),
        (Method::DELETE, None),
    ] {
        let res = server.request(method.clone(), &format!("/articles/{}", missing), body.as_ref(), &[])?;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{} should be 404", method);
        assert_eq!(res.error_code(), Some("not_found"));
    }

    let res = server.request(Method::GET, "/nosuchtess/count", None, &[])?;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[test]
fn replace_patch_and_delete() -> Result<()> {
    let server = TestServer::start()?;
    let id = server.insert("articles", &json!({ "title": "Draft", "views": 1, "author": "ada" }))?;

    let res = server.request(Method::PATCH, &format!("/articles/{}", id), Some(&json!({ "views": 2, "author": null })), &[])?;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body, json!({ "id": id, "title": "Draft", "views": 2 }));

    let res = server.request(Method::PUT, &format!("/articles/{}", id), Some(&json!({ "title": "Final" })), &[])?;
    assert_eq!(res.body, json!({ "id": id, "title": "Final" }));

    let res = server.request(Method::DELETE, &format!("/articles/{}", id), None, &[])?;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(server.get_doc("articles", &id)?.is_none());
    Ok(())
}

#[test]
fn bad_input_returns_400_json() -> Result<()> {
    let server = TestServer::start()?;
    let res = server.request(Method::POST, "/articles", Some(&json!([1, 2])), &[])?;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.error_code(), Some("invalid_request"));

    let res = server.request(Method::POST, "/articles?ttl=soon", Some(&json!({ "a": 1 })), &[])?;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.error_code(), Some("invalid_request"));
    Ok(())
}

#[test]
fn list_pages_through_documents_in_id_order() -> Result<()> {
    let server = TestServer::start()?;
    let docs: Vec<Value> = (0..25).map(|i| json!({ "n": i })).collect();
    let res = server.request(Method::POST, "/items/_bulk", Some(&json!(docs)), &[])?;
    let mut expected = ids_of(&res.body);
    expected.sort();

    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    let mut pages = 0;
    loop {
        let path = match &after {
            Some(a) => format!("/items?limit=10&after={}", a),
            None => "/items?limit=10".to_string(),
        };
        let res = server.request(Method::GET, &path, None, &[])?;
        assert_eq!(res.status, StatusCode::OK);
        pages += 1;
        for doc in res.body["documents"].as_array().unwrap() {
            seen.push(doc["id"].as_str().unwrap().to_string());
        }
        match res.body["next"].as_str() {
            Some(next) => after = Some(next.to_string()),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(seen, expected);
    Ok(())
}

#[test]
fn flush_requires_post() -> Result<()> {
    let server = TestServer::start()?;
    assert_eq!(server.request(Method::GET, "/flush", None, &[])?.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(server.request(Method::POST, "/flush", None, &[])?.status, StatusCode::OK);
    Ok(())
}

// ---------------------------------------------------------------------------
// Tessellations
// ---------------------------------------------------------------------------

#[test]
fn tessellation_lifecycle() -> Result<()> {
    let mut server = TestServer::start()?;

    let res = server.request(Method::POST, "/tessellations", Some(&json!({ "name": "blog" })), &[])?;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["kind"], "user");

    let res = server.request(Method::POST, "/tessellations", Some(&json!({ "name": "blog" })), &[])?;
    assert_eq!(res.status, StatusCode::CONFLICT);

    let res = server.request(Method::POST, "/tessellations", Some(&json!({ "name": "../etc" })), &[])?;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Empty tessellations survive restarts.
    server.restart()?;
    let res = server.request(Method::GET, "/tessellations", None, &[])?;
    let names: Vec<&str> = res.body["tessellations"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"blog"), "{:?}", names);
    assert!(names.contains(&"users"));

    server.insert("blog", &json!({ "title": "post" }))?;
    let res = server.request(Method::GET, "/tessellations/blog", None, &[])?;
    assert_eq!(res.body["document_count"], 1);

    assert_eq!(server.request(Method::DELETE, "/tessellations/blog", None, &[])?.status, StatusCode::NO_CONTENT);
    assert_eq!(server.request(Method::DELETE, "/tessellations/blog", None, &[])?.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[test]
fn system_tessellations_are_protected() -> Result<()> {
    let server = TestServer::start()?;
    assert_eq!(server.request(Method::DELETE, "/tessellations/users", None, &[])?.status, StatusCode::FORBIDDEN);
    assert_eq!(server.request(Method::POST, "/_idempotency", Some(&json!({ "a": 1 })), &[])?.status, StatusCode::BAD_REQUEST);

    // The generic document API can't reach the users tessellation.
    let users = server.request(Method::GET, "/users", None, &[])?;
    let admin_id = users.body["users"][0]["id"].as_str().unwrap().to_string();
    let res = server.request(Method::GET, &format!("/users/{}", admin_id), None, &[])?;
    assert!(res.body.get("password_hash").is_none() && res.body.get("password").is_none());
    Ok(())
}

// ---------------------------------------------------------------------------
// Bulk
// ---------------------------------------------------------------------------

#[test]
fn bulk_insert_is_atomic() -> Result<()> {
    let mut server = TestServer::start()?;
    let docs: Vec<Value> = (0..500).map(|i| json!({ "n": i, "status": "new" })).collect();
    let res = server.request(Method::POST, "/items/_bulk", Some(&json!({ "documents": docs })), &[])?;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["count"], 500);
    assert_eq!(server.count("items")?, 500);

    // One bad item rejects the whole batch.
    let res = server.request(Method::POST, "/items/_bulk", Some(&json!([{ "ok": true }, "not an object"])), &[])?;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert!(res.body.pointer("/error/message").unwrap().as_str().unwrap().contains("documents[1]"));
    assert_eq!(server.count("items")?, 500);

    // The batch is one WAL record; it survives a crash intact.
    server.crash_and_restart()?;
    assert_eq!(server.count("items")?, 500);
    Ok(())
}

#[test]
fn bulk_accepts_bodies_over_two_megabytes() -> Result<()> {
    let server = TestServer::start()?;
    let filler = "x".repeat(1000);
    let docs: Vec<Value> = (0..3000).map(|i| json!({ "n": i, "filler": filler })).collect();
    let res = server.request(Method::POST, "/big/_bulk", Some(&json!(docs)), &[])?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(server.count("big")?, 3000);
    Ok(())
}

#[test]
fn bulk_patch_and_replace_by_id() -> Result<()> {
    let server = TestServer::start()?;
    let res = server.request(Method::POST, "/items/_bulk", Some(&json!([{ "n": 1 }, { "n": 2 }, { "n": 3 }])), &[])?;
    let ids = ids_of(&res.body);

    let patch: Vec<Value> = ids.iter().map(|id| json!({ "id": id, "checked": true })).collect();
    let res = server.request(Method::PATCH, "/items/_bulk", Some(&json!(patch)), &[])?;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["count"], 3);
    for id in &ids {
        assert_eq!(server.get_doc("items", id)?.unwrap()["checked"], true);
    }

    // A missing id fails the whole request and changes nothing.
    let res = server.request(
        Method::PUT,
        "/items/_bulk",
        Some(&json!([{ "id": ids[0], "n": 100 }, { "id": "01JTY87RVJ9B5863KMB2YD896B", "n": 200 }])),
        &[],
    )?;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert_eq!(server.get_doc("items", &ids[0])?.unwrap()["n"], 1);

    // Duplicate ids are rejected.
    let res = server.request(Method::PATCH, "/items/_bulk", Some(&json!([{ "id": ids[0] }, { "id": ids[0] }])), &[])?;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[test]
fn update_by_filter() -> Result<()> {
    let server = TestServer::start()?;
    let mut docs: Vec<Value> = (0..10).map(|i| json!({ "n": i, "status": "draft" })).collect();
    docs.extend((0..5).map(|i| json!({ "n": i, "status": "published" })));
    server.request(Method::POST, "/posts/_bulk", Some(&json!(docs)), &[])?;

    let update = json!({ "filter": { "status": "draft" }, "update": { "status": "published", "reviewed": true } });
    let res = server.request(Method::POST, "/posts/_update", Some(&update), &[])?;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body, json!({ "matched": 10, "modified": 10 }));

    let res = server.request(Method::POST, "/posts/_update", Some(&update), &[])?;
    assert_eq!(res.body, json!({ "matched": 0, "modified": 0 }));

    // An empty filter matches everything; unchanged documents aren't rewritten.
    let res = server.request(Method::POST, "/posts/_update", Some(&json!({ "filter": {}, "update": { "reviewed": true } })), &[])?;
    assert_eq!(res.body, json!({ "matched": 15, "modified": 5 }));
    Ok(())
}

// ---------------------------------------------------------------------------
// Idempotency
// ---------------------------------------------------------------------------

#[test]
fn idempotent_insert_replays_and_survives_a_crash() -> Result<()> {
    let mut server = TestServer::start()?;
    let body = json!({ "title": "once" });
    let key = [("Idempotency-Key", "order-123")];

    let first = server.request(Method::POST, "/orders", Some(&body), &key)?;
    assert_eq!(first.status, StatusCode::CREATED);
    assert!(!first.replayed());

    let second = server.request(Method::POST, "/orders", Some(&body), &key)?;
    assert_eq!(second.status, StatusCode::CREATED);
    assert!(second.replayed());
    assert_eq!(second.body, first.body);
    assert_eq!(server.count("orders")?, 1);

    server.crash_and_restart()?;
    let third = server.request(Method::POST, "/orders", Some(&body), &key)?;
    assert!(third.replayed());
    assert_eq!(third.body["id"], first.body["id"]);
    assert_eq!(server.count("orders")?, 1);

    // Same key, different request.
    let res = server.request(Method::POST, "/orders", Some(&json!({ "title": "twice" })), &key)?;
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(res.error_code(), Some("idempotency_key_reused"));
    Ok(())
}

#[test]
fn idempotent_bulk_and_delete() -> Result<()> {
    let server = TestServer::start()?;
    let docs = json!([{ "n": 1 }, { "n": 2 }]);
    let key = [("Idempotency-Key", "batch-1")];
    let first = server.request(Method::POST, "/items/_bulk", Some(&docs), &key)?;
    let second = server.request(Method::POST, "/items/_bulk", Some(&docs), &key)?;
    assert!(second.replayed());
    assert_eq!(first.body, second.body);
    assert_eq!(server.count("items")?, 2);

    let id = ids_of(&first.body)[0].clone();
    let key = [("Idempotency-Key", "delete-1")];
    let first = server.request(Method::DELETE, &format!("/items/{}", id), None, &key)?;
    let second = server.request(Method::DELETE, &format!("/items/{}", id), None, &key)?;
    assert_eq!(first.status, StatusCode::NO_CONTENT);
    assert_eq!(second.status, StatusCode::NO_CONTENT, "a replayed delete returns the original result");
    assert!(second.replayed());
    Ok(())
}

#[test]
fn concurrent_requests_with_one_key_write_once() -> Result<()> {
    let server = TestServer::start()?;
    let body = json!({ "title": "race" });
    let statuses: Vec<StatusCode> = thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|_| s.spawn(|| server.request(Method::POST, "/orders", Some(&body), &[("Idempotency-Key", "race-1")]).unwrap().status))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    assert!(statuses.iter().all(|s| *s == StatusCode::CREATED || *s == StatusCode::CONFLICT), "{:?}", statuses);
    assert_eq!(server.count("orders")?, 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// Users and roles
// ---------------------------------------------------------------------------

#[test]
fn roles_are_listed() -> Result<()> {
    let server = TestServer::start()?;
    let res = server.request(Method::GET, "/roles", None, &[])?;
    let names: Vec<&str> = res.body["roles"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    for role in ["admin", "reader", "writer", "owner"] {
        assert!(names.contains(&role), "{:?}", names);
    }
    assert_eq!(server.request(Method::GET, "/roles/admin", None, &[])?.status, StatusCode::OK);
    assert_eq!(server.request(Method::GET, "/roles/nope", None, &[])?.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[test]
fn user_lifecycle() -> Result<()> {
    let server = TestServer::start()?;
    let new_user = json!({
        "login": "ada",
        "password": "correct horse battery",
        "email_address": "ada@example.com",
        "roles": [{ "name": "writer", "permissions": ["articles"] }]
    });

    let res = server.request(Method::POST, "/users", Some(&new_user), &[])?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let id = res.body["id"].as_str().unwrap().to_string();
    assert!(res.body.get("password").is_none() && res.body.get("password_hash").is_none());
    assert_eq!(res.body["roles"][0]["name"], "writer");

    assert_eq!(server.request(Method::POST, "/users", Some(&new_user), &[])?.status, StatusCode::CONFLICT);

    let mut weak = new_user.clone();
    weak["login"] = json!("bob");
    weak["password"] = json!("short");
    assert_eq!(server.request(Method::POST, "/users", Some(&weak), &[])?.status, StatusCode::BAD_REQUEST);

    let mut bad_role = new_user.clone();
    bad_role["login"] = json!("carol");
    bad_role["roles"] = json!([{ "name": "superuser" }]);
    assert_eq!(server.request(Method::POST, "/users", Some(&bad_role), &[])?.status, StatusCode::BAD_REQUEST);

    // Lookup by id or login.
    assert_eq!(server.request(Method::GET, &format!("/users/{}", id), None, &[])?.body["login"], "ada");
    assert_eq!(server.request(Method::GET, "/users/ADA", None, &[])?.body["id"], id.as_str());

    let res = server.request(Method::PATCH, "/users/ada", Some(&json!({ "email_address": "ada@hexdb.ai", "password": "another long password" })), &[])?;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["email_address"], "ada@hexdb.ai");

    let res = server.request(Method::PUT, "/users/ada", Some(&json!({ "email_address": "x@y.z" })), &[])?;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "PUT requires email_address and roles");

    let users = server.request(Method::GET, "/users", None, &[])?;
    assert_eq!(users.body["users"].as_array().unwrap().len(), 2);

    assert_eq!(server.request(Method::DELETE, "/users/ada", None, &[])?.status, StatusCode::NO_CONTENT);
    assert_eq!(server.request(Method::DELETE, "/users/ada", None, &[])?.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[test]
fn last_admin_is_protected() -> Result<()> {
    let server = TestServer::start()?;
    let res = server.request(Method::DELETE, "/users/hexdbadmin", None, &[])?;
    assert_eq!(res.status, StatusCode::CONFLICT);

    let res = server.request(Method::PATCH, "/users/hexdbadmin", Some(&json!({ "is_locked": true })), &[])?;
    assert_eq!(res.status, StatusCode::CONFLICT);

    // With a second admin, the first can be removed.
    let second = json!({
        "login": "root2", "password": "a long enough password", "email_address": "root2@example.com",
        "roles": [{ "name": "admin", "permissions": ["*"] }]
    });
    assert_eq!(server.request(Method::POST, "/users", Some(&second), &[])?.status, StatusCode::CREATED);
    assert_eq!(server.request(Method::DELETE, "/users/hexdbadmin", None, &[])?.status, StatusCode::NO_CONTENT);
    Ok(())
}

#[test]
fn idempotent_user_creation() -> Result<()> {
    let server = TestServer::start()?;
    let new_user = json!({ "login": "eve", "password": "long enough password", "email_address": "eve@example.com" });
    let key = [("Idempotency-Key", "user-eve")];
    let first = server.request(Method::POST, "/users", Some(&new_user), &key)?;
    let second = server.request(Method::POST, "/users", Some(&new_user), &key)?;
    assert_eq!(first.status, StatusCode::CREATED);
    assert_eq!(second.status, StatusCode::CREATED, "a retry replays instead of reporting a duplicate login");
    assert!(second.replayed());
    assert_eq!(first.body, second.body);
    Ok(())
}

// ---------------------------------------------------------------------------
// GraphQL and filters
// ---------------------------------------------------------------------------

#[test]
fn graphql_over_http() -> Result<()> {
    let server = TestServer::start()?;
    let docs: Vec<Value> = (1..=5).map(|n| json!({ "n": n })).collect();
    server.request(Method::POST, "/posts/_bulk", Some(&json!(docs)), &[])?;

    let query = json!({
        "query": "{ count(tessellation: \"posts\", filter: { n: { _gte: 2 } }) documents(tessellation: \"posts\", sort: [{ field: \"n\", descending: true }], limit: 2) { total documents { field(path: \"n\") } } }",
        "variables": {}
    });
    let res = server.request(Method::POST, "/graphql", Some(&query), &[])?;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body.get("errors").is_none(), "{}", res.body);
    assert_eq!(res.body["data"]["count"], 4);
    assert_eq!(res.body["data"]["documents"]["total"], 5);
    assert_eq!(res.body["data"]["documents"]["documents"], json!([{ "field": 5 }, { "field": 4 }]));

    // Errors come back in the GraphQL errors array with a code.
    let res = server.request(Method::POST, "/graphql", Some(&json!({ "query": "{ documents(tessellation: \"users\") { total } }" })), &[])?;
    assert_eq!(res.body.pointer("/errors/0/extensions/code").and_then(Value::as_str), Some("FORBIDDEN"));

    // GraphiQL is served as a fallback.
    let page = server.get("/graphql")?;
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page.text()?.to_lowercase().contains("graphiql"));
    Ok(())
}

#[test]
fn rest_update_supports_filter_operators() -> Result<()> {
    let server = TestServer::start()?;
    let docs: Vec<Value> = (1..=10).map(|n| json!({ "n": n, "tags": if n % 2 == 0 { json!(["even"]) } else { json!(["odd"]) } })).collect();
    server.request(Method::POST, "/nums/_bulk", Some(&json!(docs)), &[])?;

    let update = json!({ "filter": { "n": { "$gt": 3 }, "tags": "even" }, "update": { "big_even": true } });
    let res = server.request(Method::POST, "/nums/_update", Some(&update), &[])?;
    assert_eq!(res.body, json!({ "matched": 4, "modified": 4 }), "6, 8 and 10 plus 4");

    let res = server.request(Method::POST, "/nums/_update", Some(&json!({ "filter": { "n": { "$bogus": 1 } }, "update": { "x": 1 } })), &[])?;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    Ok(())
}

// ---------------------------------------------------------------------------
// REST filtering, sorting and paging; GraphQL idempotency over HTTP
// ---------------------------------------------------------------------------

/// Percent-encode a query-string value.
fn enc(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{:02X}", b)
            }
        })
        .collect()
}

fn field_values(body: &Value, field: &str) -> Vec<Value> {
    body["documents"].as_array().unwrap().iter().map(|d| d[field].clone()).collect()
}

fn seed_numbers(server: &TestServer) -> Result<()> {
    let docs: Vec<Value> = (1..=12)
        .map(|n| json!({ "n": n, "group": if n % 2 == 0 { "even" } else { "odd" }, "name": format!("item-{:02}", n) }))
        .collect();
    let res = server.request(Method::POST, "/nums/_bulk", Some(&json!(docs)), &[])?;
    assert_eq!(res.status, StatusCode::CREATED);
    Ok(())
}

#[test]
fn rest_list_filters_sorts_and_pages() -> Result<()> {
    let server = TestServer::start()?;
    seed_numbers(&server)?;

    // Filter + sort descending + offset paging.
    let filter = enc(r#"{"group":"even","n":{"$gt":2}}"#);
    let res = server.request(Method::GET, &format!("/nums?filter={}&sort=-n&limit=2&offset=1", filter), None, &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["total"], 5, "4, 6, 8, 10, 12");
    assert_eq!(field_values(&res.body, "n"), vec![json!(10), json!(8)]);
    assert_eq!(res.body["next"], Value::Null, "sorted results page with offset");

    // Multiple sort keys.
    let res = server.request(Method::GET, "/nums?sort=group,-n&limit=3", None, &[])?;
    assert_eq!(field_values(&res.body, "n"), vec![json!(12), json!(10), json!(8)]);

    // Unsorted: cursor paging still works, now with a total.
    let res = server.request(Method::GET, "/nums?limit=5", None, &[])?;
    assert_eq!(res.body["total"], 12);
    assert!(res.body["next"].is_string());

    // Count with a filter.
    let res = server.request(Method::GET, &format!("/nums/count?filter={}", enc(r#"{"group":"odd"}"#)), None, &[])?;
    assert_eq!(res.body, json!({ "count": 6 }));

    // Errors are 400s.
    for path in [
        format!("/nums?filter={}", enc("{not json")),
        format!("/nums?filter={}", enc(r#"{"n":{"$bogus":1}}"#)),
        "/nums?sort=-".to_string(),
        "/nums?sort=n&after=01JTY87RVJ9B5863KMB2YD896B".to_string(),
        "/nums?limit=5000".to_string(),
        "/nums?colour=red".to_string(),
    ] {
        let res = server.request(Method::GET, &path, None, &[])?;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{} -> {}", path, res.body);
    }
    Ok(())
}

#[test]
fn rest_query_endpoint_accepts_a_json_body() -> Result<()> {
    let server = TestServer::start()?;
    seed_numbers(&server)?;

    let body = json!({
        "filter": { "$or": [{ "n": { "$lte": 2 } }, { "name": { "$endsWith": "12" } }] },
        "sort": [{ "field": "n", "descending": true }],
        "limit": 10
    });
    let res = server.request(Method::POST, "/nums/_query", Some(&body), &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["total"], 3);
    assert_eq!(field_values(&res.body, "n"), vec![json!(12), json!(2), json!(1)]);

    // Sort may also be the compact string form.
    let res = server.request(Method::POST, "/nums/_query", Some(&json!({ "sort": "-n", "limit": 1 })), &[])?;
    assert_eq!(field_values(&res.body, "n"), vec![json!(12)]);

    let res = server.request(Method::POST, "/nums/_query", Some(&json!({ "filters": {} })), &[])?;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "unknown keys are rejected");
    Ok(())
}

#[test]
fn graphql_idempotency_over_http() -> Result<()> {
    let server = TestServer::start()?;
    let body = json!({
        "query": "mutation { insertDocument(tessellation: \"orders\", data: { qty: 1 }, idempotencyKey: \"gql-1\") { id } }"
    });
    let first = server.request(Method::POST, "/graphql", Some(&body), &[])?;
    let second = server.request(Method::POST, "/graphql", Some(&body), &[])?;
    assert!(!first.replayed());
    assert!(second.replayed(), "the HTTP response is marked as replayed");
    assert_eq!(second.body["data"], first.body["data"]);
    assert_eq!(second.body["extensions"]["idempotentReplays"], json!(["insertDocument"]));
    assert_eq!(server.count("orders")?, 1);
    Ok(())
}

#[test]
fn status_reports_operations_and_history() -> Result<()> {
    let server = TestServer::start()?;
    let id = server.insert("items", &json!({ "n": 1 }))?;
    server.insert("items", &json!({ "n": 2 }))?;
    server.get_doc("items", &id)?;
    server.request(Method::GET, "/items?sort=-n", None, &[])?;

    let status = server.request(Method::GET, "/status", None, &[])?;
    let ops = &status.body["operations"];
    assert!(ops["writes_total"].as_u64().unwrap() >= 2, "{}", ops);
    assert!(ops["reads_total"].as_u64().unwrap() >= 1, "{}", ops);
    assert!(ops["queries_total"].as_u64().unwrap() >= 1, "{}", ops);
    assert!(status.body["network"].get("query_endpoint").is_none(), "the retired port is gone from /status");

    // A sample is recorded at startup.
    let history = server.request(Method::GET, "/status/history?minutes=5", None, &[])?;
    assert_eq!(history.status, StatusCode::OK);
    assert_eq!(history.body["interval_seconds"], 15);
    let samples = history.body["samples"].as_array().unwrap();
    assert!(!samples.is_empty());
    for key in ["timestamp", "documents", "memory_bytes", "disk_bytes", "writes_total"] {
        assert!(samples[0].get(key).is_some(), "sample is missing {}", key);
    }

    assert_eq!(server.request(Method::GET, "/status/history?minutes=soon", None, &[])?.status, StatusCode::BAD_REQUEST);
    Ok(())
}
