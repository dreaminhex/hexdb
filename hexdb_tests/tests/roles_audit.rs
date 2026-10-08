//! Custom roles (permission sets) and the persistent audit trail.

use anyhow::Result;
use hexdb_tests::{TestServer, TEST_ADMIN_LOGIN};
use reqwest::Method;
use serde_json::{json, Value};

fn status_as(server: &TestServer, key: &str, method: Method, path: &str, body: Option<&Value>) -> Result<u16> {
    Ok(server.request_as(Some(key), method, path, body, &[])?.status.as_u16())
}

#[test]
fn custom_roles_grant_exactly_their_permission_sets() -> Result<()> {
    let server = TestServer::start()?;
    server.insert("events", &json!({ "seed": true }))?;

    // Validation: unknown permissions, empty sets, built-in names.
    let bad = server.request(Method::POST, "/roles", Some(&json!({ "name": "x", "permissions": ["fly"] })), &[])?;
    assert_eq!(bad.status, 400, "{}", bad.body);
    assert_eq!(server.request(Method::POST, "/roles", Some(&json!({ "name": "x", "permissions": [] })), &[])?.status, 400);
    assert_eq!(server.request(Method::POST, "/roles", Some(&json!({ "name": "writer", "permissions": ["read"] })), &[])?.status, 409);
    assert_eq!(server.request(Method::PUT, "/roles/reader", Some(&json!({ "permissions": ["admin"] })), &[])?.status, 409, "built-ins are fixed");

    // A write-only role.
    let created = server.request(Method::POST, "/roles", Some(&json!({ "name": "appender", "description": "Append events", "permissions": ["write"] })), &[])?;
    assert_eq!(created.status, 201, "{}", created.body);
    assert_eq!(created.body["permissions"], json!(["write"]));
    let key = server.user_with_roles("ingest", json!([{ "name": "appender", "tessellations": ["events"] }]))?;
    assert_eq!(status_as(&server, &key, Method::POST, "/events", Some(&json!({ "n": 1 })))?, 201);
    assert_eq!(status_as(&server, &key, Method::GET, "/events", None)?, 403, "write without read");
    assert_eq!(status_as(&server, &key, Method::POST, "/other", Some(&json!({ "n": 1 })))?, 403, "only on the granted tessellation");
    assert_eq!(status_as(&server, &key, Method::GET, "/status", None)?, 403);

    // Changing the role applies to the next request.
    let changed = server.request(Method::PATCH, "/roles/appender", Some(&json!({ "permissions": ["read", "write"] })), &[])?;
    assert_eq!(changed.status, 200, "{}", changed.body);
    assert_eq!(status_as(&server, &key, Method::GET, "/events", None)?, 200);

    // The signed-in user sees their resolved permissions.
    let me = server.request_as(Some(&key), Method::GET, "/auth/me", None, &[])?;
    assert_eq!(me.body["grants"][0]["permissions"], json!(["read", "write"]), "{}", me.body);

    // A role in use can't be deleted; an unused one can.
    assert_eq!(server.request(Method::DELETE, "/roles/appender", None, &[])?.status, 409);
    assert_eq!(server.request(Method::DELETE, "/roles/reader", None, &[])?.status, 409);
    server.request(Method::POST, "/roles", Some(&json!({ "name": "spare", "permissions": ["logs"] })), &[])?;
    assert_eq!(server.request(Method::DELETE, "/roles/spare", None, &[])?.status, 204);

    // The role list includes the built-ins and the permission catalog.
    let roles = server.request(Method::GET, "/roles", None, &[])?;
    let names: Vec<&str> = roles.body["roles"].as_array().unwrap().iter().filter_map(|r| r["name"].as_str()).collect();
    for name in ["admin", "reader", "writer", "owner", "operator", "auditor", "appender"] {
        assert!(names.contains(&name), "{} in {:?}", name, names);
    }
    assert_eq!(roles.body["permissions"].as_array().unwrap().len(), 9);
    Ok(())
}

#[test]
fn operator_and_auditor_roles_are_limited_to_their_areas() -> Result<()> {
    let server = TestServer::start()?;
    server.insert("notes", &json!({ "secret": 1 }))?;
    let operator = server.user_with_roles("opal", json!([{ "name": "operator" }]))?;
    let auditor = server.user_with_roles("audrey", json!([{ "name": "auditor" }]))?;

    for (path, method, op, au) in [
        ("/status", Method::GET, 200, 200),
        ("/status/history", Method::GET, 200, 200),
        ("/logs", Method::GET, 200, 403),
        ("/plugins", Method::GET, 200, 403),
        ("/flush", Method::POST, 200, 403),
        ("/audit", Method::GET, 403, 200),
        ("/users", Method::GET, 403, 403),
        ("/notes", Method::GET, 403, 403),
        ("/roles", Method::GET, 403, 403),
    ] {
        assert_eq!(status_as(&server, &operator, method.clone(), path, None)?, op, "operator {}", path);
        assert_eq!(status_as(&server, &auditor, method, path, None)?, au, "auditor {}", path);
    }
    Ok(())
}

fn events(server: &TestServer, query: &str) -> Result<Vec<Value>> {
    let res = server.request(Method::GET, &format!("/audit{}", query), None, &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    Ok(res.body["events"].as_array().cloned().unwrap_or_default())
}

#[test]
fn security_events_are_audited_and_survive_restarts() -> Result<()> {
    let mut server = TestServer::start()?;

    // A failed sign-in, a user created, and a refused request.
    let failed = server.request_as(None, Method::POST, "/auth/login", Some(&json!({ "login": "mallory", "password": "not the password" })), &[])?;
    assert_eq!(failed.status, 401);
    let reader = server.user_with_roles("rhea", json!([{ "name": "reader", "tessellations": ["notes"] }]))?;
    assert_eq!(status_as(&server, &reader, Method::DELETE, "/tessellations/notes", None)?, 403);

    let login_failures = events(&server, "?action=auth.login&outcome=failed")?;
    assert_eq!(login_failures.len(), 1, "{:?}", login_failures);
    assert_eq!(login_failures[0]["target"], "mallory");
    assert_eq!(login_failures[0]["client"], "127.0.0.1");

    let created = events(&server, "?action=user.create")?;
    assert_eq!(created[0]["actor"], TEST_ADMIN_LOGIN);
    assert_eq!(created[0]["target"], "rhea");

    let denied = events(&server, "?action=access.denied")?;
    assert_eq!(denied[0]["actor"], "rhea");
    assert_eq!(denied[0]["target"], "/tessellations/notes");

    // Prefix filter, newest first.
    let auth = events(&server, "?action=auth.")?;
    assert!(auth.iter().all(|e| e["action"].as_str().unwrap().starts_with("auth.")));
    let times: Vec<i64> = auth.iter().map(|e| e["time"].as_i64().unwrap()).collect();
    assert!(times.windows(2).all(|w| w[0] >= w[1]), "newest first: {:?}", times);
    assert!(auth.iter().any(|e| e["action"] == "auth.key_create" && e["target"] == "rhea"));

    // Readers can't see the audit trail, and it isn't a document route.
    assert_eq!(status_as(&server, &reader, Method::GET, "/audit", None)?, 403);
    assert_eq!(server.request(Method::GET, "/_audit", None, &[])?.status.as_u16() / 100, 4);

    // Persisted: still there after a crash.
    server.crash_and_restart()?;
    assert_eq!(events(&server, "?action=auth.login&outcome=failed")?.len(), 1);
    assert!(!events(&server, "?actor=rhea")?.is_empty());
    Ok(())
}
