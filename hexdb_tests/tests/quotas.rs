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
    Ok(())
}
