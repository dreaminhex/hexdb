//! Upload quotas: per user (or the lattice's default), in bytes and files,
//! counting what a user's uploads keep stored, the trash included.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::{Method, StatusCode};
use serde_json::json;

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

fn put(server: &TestServer, key: &str, path: &str, data: Vec<u8>) -> Result<(StatusCode, serde_json::Value)> {
    let res = server.send_bytes_as(Some(key), Method::PUT, &format!("/media/_files/{}", path), data)?;
    Ok((res.status, res.body))
}

#[test]
fn uploads_stop_at_the_quota_and_deleting_frees_room() -> Result<()> {
    let server = TestServer::start()?;
    assert!(server.request(Method::POST, "/tessellations", Some(&json!({ "name": "media", "kind": "files" })), &[])?.status.is_success());
    let key = server.user_with_roles("una", json!([{ "name": "writer", "tessellations": ["media"] }]))?;
    let res = server.request(Method::PATCH, "/users/una", Some(&json!({ "quota_mb": 1, "quota_files": 3 })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    assert_eq!(res.body["quota_mb"], 1);

    // Bytes: 600 KB fits, another 600 KB doesn't.
    assert_eq!(put(&server, &key, "a.bin", bytes(600 * 1024, 1))?.0, StatusCode::CREATED);
    let (status, body) = put(&server, &key, "b.bin", bytes(600 * 1024, 2))?;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE, "{}", body);
    assert_eq!(body["error"]["code"], "quota_exceeded");
    let quota = server.request_as(Some(&key), Method::GET, "/auth/quota", None, &[])?.body;
    assert_eq!(quota["limit_bytes"], 1024 * 1024);
    assert_eq!(quota["used_bytes"], 600 * 1024);
    assert_eq!(quota["used_files"], 1);

    // A resumable upload that declares too much is refused before any of it is sent.
    let res = server.request_as(Some(&key), Method::POST, "/media/_uploads", Some(&json!({ "path": "big.bin", "size": 5 * 1024 * 1024 })), &[])?;
    assert_eq!(res.status, StatusCode::INSUFFICIENT_STORAGE, "{}", res.body);

    // Files: three at most.
    assert_eq!(put(&server, &key, "c.txt", b"c".to_vec())?.0, StatusCode::CREATED);
    assert_eq!(put(&server, &key, "d.txt", b"d".to_vec())?.0, StatusCode::CREATED);
    assert_eq!(put(&server, &key, "e.txt", b"e".to_vec())?.0, StatusCode::INSUFFICIENT_STORAGE, "a fourth file");
    // A new version isn't a new file (but its bytes count).
    assert_eq!(put(&server, &key, "c.txt", b"c, revised".to_vec())?.0, StatusCode::OK);

    // The trash still counts; purging frees the room.
    let listed = server.request(Method::POST, "/media/_query", Some(&json!({ "filter": { "name": "a.bin" } })), &[])?.body;
    let id = listed["documents"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(server.request_as(Some(&key), Method::DELETE, &format!("/media/{}", id), None, &[])?.status, StatusCode::NO_CONTENT);
    assert_eq!(put(&server, &key, "b.bin", bytes(600 * 1024, 2))?.0, StatusCode::INSUFFICIENT_STORAGE, "trashed files still count");
    assert!(server.request(Method::DELETE, &format!("/media/_trash/{}", id), None, &[])?.status.is_success());
    let res = server.request(Method::PATCH, "/users/una", Some(&json!({ "quota_files": 0 })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    assert_eq!(put(&server, &key, "b.bin", bytes(600 * 1024, 2))?.0, StatusCode::CREATED, "room again after purging (and no file limit)");

    // The lattice's default applies to users without their own; administrators have none.
    let res = server.request(Method::PUT, "/config", Some(&json!({ "changes": { "quotas.default_files": 1 } })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    let vic = server.user_with_roles("vic", json!([{ "name": "writer", "tessellations": ["media"] }]))?;
    assert_eq!(put(&server, &vic, "v1.txt", b"1".to_vec())?.0, StatusCode::CREATED);
    assert_eq!(put(&server, &vic, "v2.txt", b"2".to_vec())?.0, StatusCode::INSUFFICIENT_STORAGE);
    for i in 0..3 {
        let res = server.send_bytes(Method::PUT, &format!("/media/_files/admin-{}.txt", i), b"x".to_vec(), &[])?;
        assert!(res.status.is_success(), "administrators aren't held to the default: {}", res.body);
    }
    // Clearing a user's own quota (null) puts them back on the default.
    let res = server.request(Method::PATCH, "/users/una", Some(&json!({ "quota_mb": null, "quota_files": null })), &[])?;
    assert!(res.body["quota_mb"].is_null(), "{}", res.body);

    // A files tessellation can have a quota of its own, whoever uploads.
    assert!(server.request(Method::POST, "/tessellations", Some(&json!({ "name": "inbox", "kind": "files" })), &[])?.status.is_success());
    let res = server.request(Method::PUT, "/tessellations/inbox/quota", Some(&json!({ "files": 2 })), &[])?;
    assert_eq!((res.status, res.body["quota"]["files"].as_u64()), (StatusCode::OK, Some(2)), "{}", res.body);
    for i in 0..2 {
        assert!(server.send_bytes(Method::PUT, &format!("/inbox/_files/{}.txt", i), b"x".to_vec(), &[])?.status.is_success());
    }
    let res = server.send_bytes(Method::PUT, "/inbox/_files/third.txt", b"x".to_vec(), &[])?;
    assert_eq!((res.status, res.error_code()), (StatusCode::INSUFFICIENT_STORAGE, Some("quota_exceeded")), "even an administrator: {}", res.body);
    assert!(res.body["error"]["message"].as_str().unwrap().contains("'inbox'"), "{}", res.body);
    let info = server.request(Method::GET, "/tessellations/inbox", None, &[])?.body;
    assert_eq!((info["quota"]["files"].as_u64(), info["quota_used"]["files"].as_u64()), (Some(2), Some(2)), "{}", info);
    assert_eq!(server.request(Method::PUT, "/tessellations/inbox/quota", Some(&json!(null)), &[])?.status, StatusCode::OK);
    assert!(server.send_bytes(Method::PUT, "/inbox/_files/third.txt", b"x".to_vec(), &[])?.status.is_success());
    // Only files tessellations have quotas.
    server.insert("notes", &json!({ "n": 1 }))?;
    assert_eq!(server.request(Method::PUT, "/tessellations/notes/quota", Some(&json!({ "mb": 1 })), &[])?.status, StatusCode::BAD_REQUEST);
    // Counting quotas gave the files tessellations an index on uploaded_by.
    let indexes = server.request(Method::GET, "/tessellations/media", None, &[])?.body;
    assert!(indexes["indexes"].as_array().unwrap().iter().any(|i| i == "uploaded_by"), "{}", indexes);
    Ok(())
}

/// Uploads committing at the same time can't share the room left: each
/// reserves its size between the quota check and the commit.
#[test]
fn concurrent_uploads_never_go_past_the_quota() -> Result<()> {
    let server = TestServer::start()?;
    assert!(server.request(Method::POST, "/tessellations", Some(&json!({ "name": "media", "kind": "files" })), &[])?.status.is_success());
    let key = server.user_with_roles("una", json!([{ "name": "writer", "tessellations": ["media"] }]))?;
    let res = server.request(Method::PATCH, "/users/una", Some(&json!({ "quota_mb": 1 })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    // Count the (empty) usage first, so every upload checks against the same cached figure.
    assert_eq!(server.request_as(Some(&key), Method::GET, "/auth/quota", None, &[])?.body["used_bytes"], 0);

    let statuses: Vec<StatusCode> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8u8).map(|i| {
            let (server, key) = (&server, &key);
            scope.spawn(move || put(server, key, &format!("f{}.bin", i), bytes(300 * 1024, i)).map(|(status, _)| status))
        }).collect();
        handles.into_iter().map(|h| h.join().expect("upload thread")).collect::<Result<Vec<_>>>()
    })?;
    let stored = statuses.iter().filter(|s| **s == StatusCode::CREATED).count();
    assert!(statuses.iter().all(|s| *s == StatusCode::CREATED || *s == StatusCode::INSUFFICIENT_STORAGE), "{:?}", statuses);
    assert_eq!(stored, 3, "three 300 KB files fit in 1 MB: {:?}", statuses);
    let quota = server.request_as(Some(&key), Method::GET, "/auth/quota", None, &[])?.body;
    assert!(quota["used_bytes"].as_u64().unwrap() <= 1024 * 1024, "{}", quota);
    Ok(())
}
