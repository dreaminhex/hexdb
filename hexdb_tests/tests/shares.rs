//! Sharing links: a file or folder for people without an account, through
//! MAR-62 codes, within what the link's maker may read, with passwords,
//! download limits, expiry, deletion and a lockout for guessing.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};

fn upload(server: &TestServer, path: &str, bytes: &[u8]) -> Result<String> {
    let res = server.send_bytes(Method::PUT, &format!("/media/_files/{}", path), bytes.to_vec(), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    Ok(res.body["id"].as_str().unwrap().to_string())
}

fn anonymous(server: &TestServer, method: Method, path: &str, body: Option<&Value>) -> Result<hexdb_tests::ApiResponse> {
    server.request_as(None, method, path, body, &[])
}

fn share(server: &TestServer, key: &str, body: Value) -> Result<hexdb_tests::ApiResponse> {
    server.request_as(Some(key), Method::POST, "/shares", Some(&body), &[])
}

#[test]
fn links_reach_only_what_they_name_as_their_maker_sees_it() -> Result<()> {
    let server = TestServer::start()?;
    assert!(server.request(Method::POST, "/tessellations", Some(&json!({ "name": "media", "kind": "files" })), &[])?.status.is_success());
    let a = upload(&server, "docs/a.txt", b"alpha")?;
    let b = upload(&server, "docs/sub/b.txt", b"bravo")?;
    let c = upload(&server, "private/c.txt", b"charlie")?;

    // Only users with the share permission make links; their folder limits still apply.
    let reader = server.user_with_roles("rita", json!([{ "name": "reader", "tessellations": ["media"] }]))?;
    assert_eq!(share(&server, &reader, json!({ "tessellation": "media", "file_id": a }))?.status, StatusCode::FORBIDDEN);
    let role = json!({ "name": "sharer", "permissions": ["read", "share"], "restrictions": { "media": { "folders": ["/docs/"] } } });
    assert!(server.request(Method::POST, "/roles", Some(&role), &[])?.status.is_success());
    let sam = server.user_with_roles("sam", json!([{ "name": "sharer", "tessellations": ["media"] }]))?;
    assert_eq!(share(&server, &sam, json!({ "tessellation": "media", "file_id": c }))?.status, StatusCode::NOT_FOUND, "outside sam's folders");

    // A file link.
    let res = share(&server, &sam, json!({ "tessellation": "media", "file_id": a, "note": "for the auditor" }))?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let code = res.body["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 8);
    assert!(code.bytes().all(|c| c.is_ascii_alphanumeric()));
    assert_eq!(res.body["path"], format!("/_s/{}", code));
    let info = anonymous(&server, Method::GET, &format!("/_s/{}", code), None)?;
    assert_eq!(info.status, StatusCode::OK, "{}", info.body);
    assert_eq!(info.body["file"]["name"], "a.txt");
    assert!(info.body.get("created_by").is_none() && info.body.get("note").is_none(), "anonymous visitors don't see who made it: {}", info.body);
    let (status, headers, bytes) = server.get_bytes_as(None, &format!("/_s/{}/content", code), &[])?;
    assert_eq!((status, bytes.as_slice()), (StatusCode::OK, b"alpha".as_slice()));
    assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
    assert_eq!(headers.get("x-robots-tag").unwrap(), "noindex, nofollow");
    // Browsers following the link land on its page.
    let no_redirects = hexdb_tests::http().redirect(reqwest::redirect::Policy::none()).build()?;
    let page = no_redirects.get(server.url(&format!("/_s/{}", code))).header("Accept", "text/html").send()?;
    assert_eq!(page.status(), StatusCode::SEE_OTHER);
    assert_eq!(page.headers().get("location").unwrap(), &format!("/ui/s/{}", code));
    // The maker sees the download.
    let mine = server.request_as(Some(&sam), Method::GET, "/shares", None, &[])?.body;
    assert_eq!(mine["shares"][0]["downloads"], 1, "{}", mine);
    assert_eq!(mine["shares"][0]["note"], "for the auditor");

    // A folder link: files under it that the maker can see, nothing else.
    let res = share(&server, &sam, json!({ "tessellation": "media", "folder": "/docs/" }))?;
    let folder = res.body["code"].as_str().unwrap().to_string();
    let listed = anonymous(&server, Method::GET, &format!("/_s/{}/files", folder), None)?.body;
    let names: Vec<&str> = listed["files"].as_array().unwrap().iter().filter_map(|f| f["name"].as_str()).collect();
    assert_eq!(names, vec!["a.txt", "b.txt"], "{}", listed);
    assert_eq!(server.get_bytes_as(None, &format!("/_s/{}/content?file={}", folder, b), &[])?.2, b"bravo");
    assert_eq!(server.get_bytes_as(None, &format!("/_s/{}/content?file={}", folder, c), &[])?.0, StatusCode::NOT_FOUND, "outside the folder");
    // A link to the whole tree still shows only the maker's folders.
    let res = share(&server, &sam, json!({ "tessellation": "media", "folder": "/" }))?;
    let root = res.body["code"].as_str().unwrap().to_string();
    let listed = anonymous(&server, Method::GET, &format!("/_s/{}/files", root), None)?.body;
    assert!(listed["files"].as_array().unwrap().iter().all(|f| f["name"] != "c.txt"), "{}", listed);

    // Password links need the password; the unlock token is bound to the link.
    let res = share(&server, &sam, json!({ "tessellation": "media", "file_id": a, "password": "correct horse battery" }))?;
    let locked = res.body["code"].as_str().unwrap().to_string();
    assert_eq!(server.get_bytes_as(None, &format!("/_s/{}/content", locked), &[])?.0, StatusCode::UNAUTHORIZED);
    assert!(anonymous(&server, Method::GET, &format!("/_s/{}", locked), None)?.body.get("file").is_none(), "nothing about the file before unlocking");
    let wrong = anonymous(&server, Method::POST, &format!("/_s/{}/unlock", locked), Some(&json!({ "password": "wrong guess here" })))?;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);
    let token = anonymous(&server, Method::POST, &format!("/_s/{}/unlock", locked), Some(&json!({ "password": "correct horse battery" })))?.body["token"].as_str().unwrap().to_string();
    assert_eq!(server.get_bytes_as(None, &format!("/_s/{}/content?t={}", locked, token), &[])?.2, b"alpha");
    assert_eq!(server.get_bytes_as(None, &format!("/_s/{}/content?t={}x", locked, token), &[])?.0, StatusCode::UNAUTHORIZED, "a tampered token");
    assert_eq!(server.get_bytes_as(None, &format!("/_s/{}/content?t={}", code, token), &[])?.0, StatusCode::OK, "a token for one link doesn't matter on an open one");

    // Download limits and expiry.
    let res = share(&server, &sam, json!({ "tessellation": "media", "file_id": a, "max_downloads": 1 }))?;
    let once = res.body["code"].as_str().unwrap().to_string();
    assert_eq!(server.get_bytes_as(None, &format!("/_s/{}/content", once), &[])?.0, StatusCode::OK);
    assert_eq!(server.get_bytes_as(None, &format!("/_s/{}/content", once), &[])?.0, StatusCode::NOT_FOUND, "used up");
    // Ranges count by the bytes they serve: skipping byte 0 doesn't get around the limit.
    let res = share(&server, &sam, json!({ "tessellation": "media", "file_id": a, "max_downloads": 2 }))?;
    let twice = res.body["code"].as_str().unwrap().to_string();
    let ranged = || server.get_bytes_as(None, &format!("/_s/{}/content", twice), &[("Range", "bytes=1-")]);
    for _ in 0..3 {
        let (status, _, bytes) = ranged()?;
        assert_eq!((status, bytes.as_slice()), (StatusCode::PARTIAL_CONTENT, &b"lpha"[..]));
    }
    // Four fifths three times is two downloads and more: used up. (Checked in the maker's
    // list: a request to a dead link would count toward this address's lockout.)
    let mine = server.request_as(Some(&sam), Method::GET, "/shares", None, &[])?.body;
    let link = mine["shares"].as_array().unwrap().iter().find(|s| s["code"] == twice.as_str()).cloned().unwrap();
    assert_eq!(link["downloads"], 2, "{}", link);
    let res = share(&server, &sam, json!({ "tessellation": "media", "file_id": a, "expires_in_days": 0.00001 }))?;
    let brief = res.body["code"].as_str().unwrap().to_string();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert_eq!(anonymous(&server, Method::GET, &format!("/_s/{}", brief), None)?.status, StatusCode::NOT_FOUND, "expired");
    assert_eq!(share(&server, &sam, json!({ "tessellation": "media", "file_id": a, "expires_in_days": 60 }))?.status, StatusCode::BAD_REQUEST, "past sharing.max_days");

    // Only the maker or an administrator deletes a link.
    let other = server.user_with_roles("otto", json!([{ "name": "sharer", "tessellations": ["media"] }]))?;
    assert_eq!(server.request_as(Some(&other), Method::DELETE, &format!("/shares/{}", code), None, &[])?.status, StatusCode::NOT_FOUND);
    assert_eq!(server.request_as(Some(&sam), Method::DELETE, &format!("/shares/{}", code), None, &[])?.status, StatusCode::NO_CONTENT);
    assert_eq!(anonymous(&server, Method::GET, &format!("/_s/{}", code), None)?.status, StatusCode::NOT_FOUND);

    // A maker who loses the share permission takes their links down with it.
    let demoted = json!({ "name": "sharer", "permissions": ["read"], "restrictions": { "media": { "folders": ["/docs/"] } } });
    assert!(server.request(Method::PUT, "/roles/sharer", Some(&demoted), &[])?.status.is_success());
    assert_eq!(anonymous(&server, Method::GET, &format!("/_s/{}/files", folder), None)?.status, StatusCode::NOT_FOUND);

    // Every step is audited.
    let audit = server.request(Method::GET, "/audit?action=share.", None, &[])?.body;
    let actions: Vec<&str> = audit["events"].as_array().unwrap().iter().filter_map(|e| e["action"].as_str()).collect();
    for action in ["share.create", "share.download", "share.unlock", "share.delete"] {
        assert!(actions.contains(&action), "{} in {:?}", action, actions);
    }
    Ok(())
}

#[test]
fn guessing_codes_locks_the_address_out() -> Result<()> {
    let server = TestServer::start()?;
    for guess in ["AAAAAAAA", "AAAAAAAB", "AAAAAAAC", "AAAAAAAD", "AAAAAAAE"] {
        assert_eq!(anonymous(&server, Method::GET, &format!("/_s/{}", guess), None)?.status, StatusCode::NOT_FOUND);
    }
    let blocked = anonymous(&server, Method::GET, "/_s/AAAAAAAF", None)?;
    assert_eq!(blocked.status, StatusCode::TOO_MANY_REQUESTS, "{}", blocked.body);
    // Malformed codes never reach the store, and other routes stay private.
    assert_eq!(anonymous(&server, Method::GET, "/shares", None)?.status, StatusCode::UNAUTHORIZED);
    assert_eq!(anonymous(&server, Method::DELETE, "/_s/AAAAAAAA", None)?.status, StatusCode::UNAUTHORIZED);
    Ok(())
}
