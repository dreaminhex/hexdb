//! File permissions: `upload`, `delete` and `restore` are separate from
//! `write` on files tessellations, and role restrictions can limit each to
//! folders.

use anyhow::Result;
use hexdb_tests::{ApiResponse, TestServer};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};

fn role(server: &TestServer, name: &str, permissions: &[&str], restrictions: Value) -> Result<()> {
    let res = server.request(Method::POST, "/roles", Some(&json!({ "name": name, "description": name, "permissions": permissions, "restrictions": restrictions })), &[])?;
    assert!(res.status.is_success(), "role {}: {}", name, res.body);
    Ok(())
}

fn put_as(server: &TestServer, token: &str, path: &str, text: &str) -> Result<ApiResponse> {
    server.send_bytes_as(Some(token), Method::PUT, &format!("/media/_files/{}", path), text.as_bytes().to_vec())
}

fn admin_upload(server: &TestServer, path: &str, text: &str) -> Result<String> {
    let res = server.send_bytes(Method::PUT, &format!("/media/_files/{}", path), text.as_bytes().to_vec(), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    Ok(res.body["id"].as_str().unwrap().to_string())
}

fn as_user(server: &TestServer, token: &str, method: Method, path: &str, body: Option<Value>) -> Result<ApiResponse> {
    server.request_as(Some(token), method, path, body.as_ref(), &[])
}

#[test]
fn upload_delete_and_restore_are_their_own_permissions() -> Result<()> {
    let server = TestServer::start()?;
    let res = server.request(Method::POST, "/tessellations", Some(&json!({ "name": "media", "kind": "files" })), &[])?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let public = admin_upload(&server, "public/readme.txt", "public file")?;
    let inbox = admin_upload(&server, "inbox/note.txt", "first")?;
    admin_upload(&server, "inbox/note.txt", "second")?;

    // `write` alone edits metadata but can't add, delete or restore files.
    role(&server, "editor", &["read", "write"], json!({}))?;
    let editor = server.user_with_roles("eddie", json!([{ "name": "editor", "tessellations": ["media"] }]))?;
    let res = as_user(&server, &editor, Method::PATCH, &format!("/media/{}", public), Some(json!({ "tags": ["docs"] })))?;
    assert!(res.status.is_success(), "{}", res.body);
    assert_eq!(put_as(&server, &editor, "public/new.txt", "x")?.status, StatusCode::FORBIDDEN);
    assert_eq!(as_user(&server, &editor, Method::DELETE, &format!("/media/{}", public), None)?.status, StatusCode::FORBIDDEN);
    assert_eq!(as_user(&server, &editor, Method::POST, &format!("/media/{}/versions/1/restore", inbox), None)?.status, StatusCode::FORBIDDEN);
    // Nor delete through a transaction.
    let res = as_user(&server, &editor, Method::POST, "/transactions", Some(json!({ "operations": [{ "op": "delete", "tessellation": "media", "id": public }] })))?;
    assert!(!res.status.is_success(), "{}", res.body);
    assert!(server.get_doc("media", &public)?.is_some(), "still there");
    // Nor create a files tessellation.
    let creator = server.user_with_roles("cora", json!([{ "name": "editor", "tessellations": ["album"] }]))?;
    let res = as_user(&server, &creator, Method::POST, "/tessellations", Some(json!({ "name": "album", "kind": "files" })))?;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    // Upload only into /inbox/, beside a role that reads everything.
    role(&server, "inbox-uploader", &["upload"], json!({ "media": { "folders": ["/inbox/"] } }))?;
    let uploader = server.user_with_roles("ulla", json!([{ "name": "inbox-uploader", "tessellations": ["media"] }, { "name": "reader", "tessellations": ["media"] }]))?;
    let res = put_as(&server, &uploader, "inbox/2026/drop.txt", "dropped")?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = put_as(&server, &uploader, "public/drop.txt", "nope")?;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = put_as(&server, &uploader, "inboxes/drop.txt", "nope")?;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "a folder, not a name prefix: {}", res.body);
    assert_eq!(as_user(&server, &uploader, Method::GET, &format!("/media/{}", public), None)?.status, StatusCode::OK, "reads everything");
    assert_eq!(as_user(&server, &uploader, Method::DELETE, &format!("/media/{}", inbox), None)?.status, StatusCode::FORBIDDEN);
    // A resumable upload outside the folder fails when it completes.
    let res = as_user(&server, &uploader, Method::POST, "/media/_uploads", Some(json!({ "path": "public/big.bin", "size": 3 })))?;
    if res.status.is_success() {
        let id = res.body["id"].as_str().unwrap().to_string();
        let part = server.send_bytes_as(Some(&uploader), Method::PATCH, &format!("/media/_uploads/{}?offset=0", id), b"abc".to_vec())?;
        assert!(part.status.is_success(), "{}", part.body);
        let done = as_user(&server, &uploader, Method::POST, &format!("/media/_uploads/{}/complete", id), None)?;
        assert_eq!(done.status, StatusCode::FORBIDDEN, "{}", done.body);
    } else {
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    }

    // Delete only in /inbox/: trash and purge there, nothing elsewhere, and no restoring.
    role(&server, "inbox-cleaner", &["delete"], json!({ "*": { "folders": ["/inbox/"] } }))?;
    let cleaner = server.user_with_roles("cleo", json!([{ "name": "inbox-cleaner", "tessellations": ["media"] }, { "name": "reader", "tessellations": ["media"] }]))?;
    assert_eq!(as_user(&server, &cleaner, Method::DELETE, &format!("/media/{}", public), None)?.status, StatusCode::FORBIDDEN);
    assert_eq!(as_user(&server, &cleaner, Method::DELETE, &format!("/media/{}", inbox), None)?.status, StatusCode::NO_CONTENT);
    assert_eq!(as_user(&server, &cleaner, Method::POST, &format!("/media/_trash/{}/restore", inbox), None)?.status, StatusCode::FORBIDDEN);

    // Restore: back from the trash, and an earlier version.
    role(&server, "restorer", &["restore"], json!({}))?;
    let restorer = server.user_with_roles("rex", json!([{ "name": "restorer", "tessellations": ["media"] }, { "name": "reader", "tessellations": ["media"] }]))?;
    let res = as_user(&server, &restorer, Method::POST, &format!("/media/_trash/{}/restore", inbox), None)?;
    assert!(res.status.is_success(), "{}", res.body);
    let res = as_user(&server, &restorer, Method::POST, &format!("/media/{}/versions/1/restore", inbox), None)?;
    assert!(res.status.is_success(), "{}", res.body);
    assert_eq!(as_user(&server, &restorer, Method::DELETE, &format!("/media/{}", inbox), None)?.status, StatusCode::FORBIDDEN);

    // The cleaner purges what it trashed.
    assert_eq!(as_user(&server, &cleaner, Method::DELETE, &format!("/media/{}", inbox), None)?.status, StatusCode::NO_CONTENT);
    assert_eq!(as_user(&server, &cleaner, Method::DELETE, &format!("/media/_trash/{}", inbox), None)?.status, StatusCode::NO_CONTENT);

    // The built-in writer role can do all of it, and create files tessellations.
    let writer = server.user_with_roles("wren", json!([{ "name": "writer", "tessellations": ["media", "album"] }]))?;
    assert_eq!(put_as(&server, &writer, "public/w.txt", "w")?.status, StatusCode::CREATED);
    assert_eq!(as_user(&server, &writer, Method::DELETE, &format!("/media/{}", public), None)?.status, StatusCode::NO_CONTENT);
    assert!(as_user(&server, &writer, Method::POST, &format!("/media/_trash/{}/restore", public), None)?.status.is_success());
    let res = as_user(&server, &writer, Method::POST, "/tessellations", Some(json!({ "name": "album", "kind": "files" })))?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // The roles catalog lists the new permissions; a bad folder is refused.
    let catalog = server.request(Method::GET, "/roles", None, &[])?.body;
    let names: Vec<&str> = catalog["permissions"].as_array().unwrap().iter().filter_map(|p| p["name"].as_str()).collect();
    for p in ["upload", "delete", "restore"] {
        assert!(names.contains(&p), "{:?}", names);
    }
    let res = server.request(Method::POST, "/roles", Some(&json!({ "name": "bad", "permissions": ["upload"], "restrictions": { "media": { "folders": ["/a/../b"] } } })), &[])?;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    Ok(())
}
