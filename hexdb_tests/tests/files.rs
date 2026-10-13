//! Files tessellations end to end: uploads, downloads with ranges, the
//! document index (filters, SQL, GraphQL), protected fields, versions, the
//! trash, garbage collection, restarts, and losing vertices.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// A WAV header followed by `len` bytes of noise.
fn wav(len: usize, seed: u64) -> Vec<u8> {
    let mut bytes = b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0\x02\0\x44\xac\0\0\x10\xb1\x02\0\x04\0\x10\0data".to_vec();
    bytes.extend_from_slice(&(len as u32).to_le_bytes());
    bytes.extend(noise(len, seed));
    bytes
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn files_server() -> Result<TestServer> {
    let server = TestServer::start()?;
    let res = server.request(Method::POST, "/tessellations", Some(&json!({ "name": "media", "kind": "files" })), &[])?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["kind"], "files");
    Ok(server)
}

fn upload(server: &TestServer, path: &str, bytes: &[u8]) -> Result<Value> {
    let res = server.send_bytes(Method::PUT, &format!("/media/_files/{}", path), bytes.to_vec(), &[])?;
    assert!(res.status.is_success(), "upload {} -> {}: {}", path, res.status, res.body);
    Ok(res.body)
}

fn download(server: &TestServer, path: &str) -> Result<Vec<u8>> {
    let (status, _, bytes) = server.get_bytes(path, &[])?;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&bytes));
    Ok(bytes)
}

#[test]
fn upload_download_and_ranges() -> Result<()> {
    let server = files_server()?;
    let content = wav(5_000_000, 1);
    let res = server.send_bytes(Method::PUT, "/media/_files/2024/Iceland/take 1.wav", content.clone(), &[])?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let doc = res.body;
    let id = doc["id"].as_str().unwrap().to_string();
    assert_eq!(doc["name"], "take 1.wav");
    assert_eq!(doc["path"], "/2024/Iceland/");
    assert_eq!(doc["size"], content.len() as u64);
    assert_eq!(doc["content_type"], "audio/x-wav");
    assert_eq!(doc["sha256"], sha256(&content));
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["content"]["compressed"], false, "lossless audio is stored as-is");
    assert!(doc["content"]["chunks"].as_u64().unwrap() >= 1);
    assert_eq!(res.headers["location"], format!("/media/{}", id));

    // By ID and by path.
    assert_eq!(download(&server, &format!("/media/{}/content", id))?, content);
    let (status, headers, by_path) = server.get_bytes("/media/_files/2024/Iceland/take%201.wav", &[])?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(by_path, content);
    assert_eq!(headers["content-type"], "audio/x-wav");
    assert_eq!(headers["accept-ranges"], "bytes");
    assert!(headers["content-disposition"].to_str()?.starts_with("inline; filename=\"take 1.wav\""));
    let etag = headers["etag"].to_str()?.to_string();
    assert_eq!(etag, format!("\"{}\"", sha256(&content)));

    // Ranges, including one that spans chunk boundaries.
    let (status, headers, part) = server.get_bytes(&format!("/media/{}/content", id), &[("range", "bytes=100-199")])?;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(part, &content[100..200]);
    assert_eq!(headers["content-range"], format!("bytes 100-199/{}", content.len()));
    let (_, _, tail) = server.get_bytes(&format!("/media/{}/content", id), &[("range", "bytes=-1000")])?;
    assert_eq!(tail, &content[content.len() - 1000..]);
    let (_, _, middle) = server.get_bytes(&format!("/media/{}/content", id), &[("range", "bytes=1000000-4500000")])?;
    assert_eq!(middle, &content[1_000_000..=4_500_000]);
    let (status, _, _) = server.get_bytes(&format!("/media/{}/content", id), &[("range", "bytes=99999999-")])?;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);

    // Conditional requests.
    let (status, _, body) = server.get_bytes(&format!("/media/{}/content", id), &[("if-none-match", etag.as_str())])?;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());

    // HTML is never shown inline.
    let page = upload(&server, "page.html", b"<script>alert(1)</script>")?;
    let (_, headers, _) = server.get_bytes(&format!("/media/{}/content", page["id"].as_str().unwrap()), &[])?;
    assert!(headers["content-disposition"].to_str()?.starts_with("attachment"));
    assert_eq!(headers["x-content-type-options"], "nosniff");

    // Empty files work.
    let empty = upload(&server, "empty.bin", b"")?;
    assert_eq!(empty["size"], 0);
    assert_eq!(download(&server, &format!("/media/{}/content", empty["id"].as_str().unwrap()))?, b"");
    Ok(())
}

#[test]
fn the_index_is_queryable_everywhere() -> Result<()> {
    let server = files_server()?;
    upload(&server, "audio/a.wav", &wav(10_000, 2))?;
    upload(&server, "audio/b.wav", &wav(20_000, 3))?;
    upload(&server, "notes/readme.txt", b"plain text notes")?;

    let res = server.request(Method::POST, "/media/_query", Some(&json!({ "filter": { "content_type": "audio/x-wav", "size": { "$gt": 15000 } } })), &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let docs = res.body["documents"].as_array().unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0]["name"], "b.wav");

    let res = server.request(Method::POST, "/sql", Some(&json!({ "sql": "SELECT name, size FROM media WHERE path = '/audio/' ORDER BY name" })), &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["rows"], json!([["a.wav", 10_044], ["b.wav", 20_044]]));

    let query = json!({ "query": "{ documents(tessellation: \"media\", filter: { content_type: { _eq: \"text/plain\" } }) { total documents { field(path: \"name\") } } }" });
    let res = server.request(Method::POST, "/graphql", Some(&query), &[])?;
    assert!(res.body.get("errors").is_none(), "{}", res.body);
    assert_eq!(res.body["data"]["documents"]["total"], 1);

    let list = server.request(Method::GET, "/tessellations", None, &[])?;
    assert!(list.body["tessellations"].as_array().unwrap().iter().any(|t| t["name"] == "media" && t["kind"] == "files"));
    assert_eq!(server.count("media")?, 3);
    Ok(())
}

#[test]
fn system_fields_are_protected() -> Result<()> {
    let server = files_server()?;
    let doc = upload(&server, "docs/a.txt", b"first")?;
    let id = doc["id"].as_str().unwrap();

    // Patching may set your own fields and rename or move, never the content fields.
    let res = server.request(
        Method::PATCH,
        &format!("/media/{}", id),
        Some(&json!({ "size": 1, "sha256": "forged", "tags": ["draft"], "name": "renamed.txt", "path": "moved//here" })),
        &[],
    )?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["size"], 5);
    assert_eq!(res.body["sha256"], sha256(b"first"));
    assert_eq!(res.body["tags"], json!(["draft"]));
    assert_eq!(res.body["name"], "renamed.txt");
    assert_eq!(res.body["path"], "/moved/here/");
    assert_eq!(download(&server, "/media/_files/moved/here/renamed.txt")?, b"first");

    // A replace keeps them too.
    let res = server.request(Method::PUT, &format!("/media/{}", id), Some(&json!({ "name": "r.txt" })), &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["size"], 5);
    assert_eq!(res.body["path"], "/moved/here/");

    // Bad names, and documents without content, are refused.
    let res = server.request(Method::PATCH, &format!("/media/{}", id), Some(&json!({ "name": "../x" })), &[])?;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = server.request(Method::POST, "/media", Some(&json!({ "name": "fake.txt" })), &[])?;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // (path, name) is unique.
    upload(&server, "moved/here/other.txt", b"other")?;
    let res = server.request(Method::PATCH, &format!("/media/{}", id), Some(&json!({ "name": "other.txt" })), &[])?;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    Ok(())
}

#[test]
fn versions_and_rollback() -> Result<()> {
    let server = files_server()?;
    let v1 = noise(3_000_000, 4);
    let mut v2 = v1.clone();
    v2[1_500_000..1_500_010].copy_from_slice(b"edited!!!!");
    let first = upload(&server, "art/cover.psd", &v1)?;
    let id = first["id"].as_str().unwrap().to_string();

    let res = server.send_bytes(Method::PUT, "/media/_files/art/cover.psd", v2.clone(), &[])?;
    assert_eq!(res.status, StatusCode::OK, "a new version: {}", res.body);
    assert_eq!(res.body["id"], id);
    assert_eq!(res.body["version"], 2);
    assert_eq!(res.body["versions"][0]["version"], 1);
    assert_eq!(res.body["versions"][0]["sha256"], sha256(&v1));

    // Uploading identical content changes nothing.
    let res = server.send_bytes(Method::PUT, "/media/_files/art/cover.psd", v2.clone(), &[])?;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.headers["x-hexdb-unchanged"], "true");
    assert_eq!(res.body["version"], 2);

    assert_eq!(download(&server, &format!("/media/{}/content", id))?, v2);
    assert_eq!(download(&server, &format!("/media/{}/content?version=1", id))?, v1);
    let listed = server.request(Method::GET, &format!("/media/{}/versions", id), None, &[])?;
    assert_eq!(listed.body["current"]["version"], 2);
    assert_eq!(listed.body["versions"].as_array().unwrap().len(), 1);

    // Roll back: version 1's content becomes version 3; version 2 is kept.
    let res = server.request(Method::POST, &format!("/media/{}/versions/1/restore", id), None, &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["version"], 3);
    assert_eq!(res.body["restored_from"], 1);
    assert_eq!(download(&server, &format!("/media/{}/content", id))?, v1);
    assert_eq!(download(&server, &format!("/media/{}/content?version=2", id))?, v2);
    let res = server.request(Method::GET, &format!("/media/{}/content?version=9", id), None, &[])?;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[test]
fn trash_restore_purge_and_garbage_collection() -> Result<()> {
    let server = files_server()?;
    let content = noise(2_500_000, 5);
    let doc = upload(&server, "keep/me.bin", &content)?;
    let id = doc["id"].as_str().unwrap().to_string();

    let res = server.request(Method::DELETE, &format!("/media/{}", id), None, &[])?;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(server.get_doc("media", &id)?.is_none());
    let trash = server.request(Method::GET, "/media/_trash", None, &[])?;
    assert_eq!(trash.body["total"], 1, "{}", trash.body);
    assert_eq!(trash.body["documents"][0]["from"], "media");
    assert!(trash.body["documents"][0]["_expires_at"].is_string());

    // Garbage collection keeps trashed content.
    server.request(Method::POST, "/compact", None, &[])?;
    let res = server.request(Method::POST, &format!("/media/_trash/{}/restore", id), None, &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(download(&server, &format!("/media/{}/content", id))?, content);

    // A clash on restore asks for another name.
    server.request(Method::DELETE, &format!("/media/{}", id), None, &[])?;
    upload(&server, "keep/me.bin", b"newcomer")?;
    let res = server.request(Method::POST, &format!("/media/_trash/{}/restore", id), None, &[])?;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = server.request(Method::POST, &format!("/media/_trash/{}/restore?name=me%20(2).bin", id), None, &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(download(&server, "/media/_files/keep/me%20(2).bin")?, content);

    // Purged content is freed by the next garbage collection.
    server.request(Method::DELETE, &format!("/media/{}", id), None, &[])?;
    let res = server.request(Method::DELETE, &format!("/media/_trash/{}", id), None, &[])?;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let compact = server.request(Method::POST, "/compact", None, &[])?;
    assert!(compact.body["chunks"]["chunks_removed"].as_u64().unwrap() >= 2, "{}", compact.body);
    let res = server.request(Method::GET, "/media/_trash", None, &[])?;
    assert_eq!(res.body["total"], 0);
    Ok(())
}

#[test]
fn files_survive_restarts_and_lost_vertices() -> Result<()> {
    let mut server = files_server()?;
    let marker = b"PLAINTEXT-MARKER-THAT-MUST-NOT-APPEAR-ON-DISK".repeat(2000);
    let content = noise(4_000_000, 6);
    let a = upload(&server, "a.bin", &content)?;
    let b = upload(&server, "b.txt", &marker)?;
    let (a, b) = (a["id"].as_str().unwrap().to_string(), b["id"].as_str().unwrap().to_string());

    // Encrypted on disk.
    for entry in walk(&server.data_dir().join("blobs")) {
        let bytes = std::fs::read(&entry)?;
        assert!(!bytes.windows(20).any(|w| w == &marker[..20]), "plaintext in {}", entry.display());
    }

    server.crash_and_restart()?;
    assert_eq!(download(&server, &format!("/media/{}/content", a))?, content);
    assert_eq!(download(&server, &format!("/media/{}/content", b))?, marker);

    // Lose two whole vertices: everything is still readable, and a scrub repairs them.
    server.stop()?;
    for v in [1, 4] {
        std::fs::remove_dir_all(server.data_dir().join("blobs").join(format!("vertex-{}", v)))?;
    }
    server.launch()?;
    let scrub = server.request(Method::POST, "/scrub", None, &[])?;
    assert_eq!(scrub.status, StatusCode::OK, "{}", scrub.body);
    assert!(scrub.body["repaired_shards"].as_u64().unwrap() >= 2, "{}", scrub.body);
    assert_eq!(scrub.body["unrecoverable"], 0);
    assert_eq!(download(&server, &format!("/media/{}/content", a))?, content);

    // Then lose two others: the repaired shards carry it.
    server.stop()?;
    for v in [0, 5] {
        std::fs::remove_dir_all(server.data_dir().join("blobs").join(format!("vertex-{}", v)))?;
    }
    server.launch()?;
    assert_eq!(download(&server, &format!("/media/{}/content", a))?, content);
    assert_eq!(download(&server, &format!("/media/{}/content", b))?, marker);
    Ok(())
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// A one-second stereo WAV with a RIFF INFO title and artist.
fn tagged_wav(title: &str, artist: &str) -> Vec<u8> {
    let chunk = |id: &[u8], data: &[u8]| {
        let mut out = id.to_vec();
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }
        out
    };
    let mut fmt = Vec::new();
    for v in [1u16, 2] {
        fmt.extend(v.to_le_bytes());
    }
    fmt.extend(44_100u32.to_le_bytes());
    fmt.extend(176_400u32.to_le_bytes());
    fmt.extend(4u16.to_le_bytes());
    fmt.extend(16u16.to_le_bytes());
    let mut info = b"INFO".to_vec();
    info.extend(chunk(b"INAM", format!("{}\0", title).as_bytes()));
    info.extend(chunk(b"IART", format!("{}\0", artist).as_bytes()));
    let mut body = b"WAVE".to_vec();
    body.extend(chunk(b"fmt ", &fmt));
    body.extend(chunk(b"LIST", &info));
    body.extend(chunk(b"data", &vec![0u8; 176_400]));
    let mut out = b"RIFF".to_vec();
    out.extend((body.len() as u32).to_le_bytes());
    out.extend(body);
    out
}

/// Wait until a file's metadata has been extracted.
fn extracted(server: &TestServer, id: &str) -> Result<Value> {
    for _ in 0..100 {
        let doc = server.get_doc("media", id)?.expect("file exists");
        if doc["extract"]["status"] == "done" {
            return Ok(doc);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    anyhow::bail!("metadata for {} was never extracted", id)
}

fn query(server: &TestServer, filter: Value) -> Result<Vec<String>> {
    let res = server.request(Method::POST, "/media/_query", Some(&json!({ "filter": filter })), &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    Ok(res.body["documents"].as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap().to_string()).collect())
}

#[test]
fn metadata_is_extracted_indexed_and_searchable() -> Result<()> {
    let server = files_server()?;
    let wav = upload(&server, "music/take5.wav", &tagged_wav("Take Five", "Dave Brubeck Quartet"))?;
    assert_eq!(wav["extract"]["status"], "pending");
    let notes = upload(&server, "docs/notes.md", b"# Release notes\n\nGlacier lagoon photography trip.\n")?;
    let (wav_id, notes_id) = (wav["id"].as_str().unwrap().to_string(), notes["id"].as_str().unwrap().to_string());

    let doc = extracted(&server, &wav_id)?;
    assert_eq!(doc["meta"]["title"], "Take Five");
    assert_eq!(doc["meta"]["artist"], "Dave Brubeck Quartet");
    assert_eq!(doc["meta"]["duration_ms"], 1000);
    assert_eq!(doc["meta"]["riff_info"]["INAM"], "Take Five");
    assert!(doc["extract"]["version"].as_u64().unwrap() >= 1, "{}", doc["extract"]);
    assert_eq!(doc["extract"]["sha256"], doc["sha256"]);
    let doc = extracted(&server, &notes_id)?;
    assert_eq!(doc["meta"]["title"], "Release notes");

    // Filters, full-text search over names and metadata, SQL and GraphQL.
    assert_eq!(query(&server, json!({ "meta.artist": "Dave Brubeck Quartet" }))?, vec!["take5.wav"]);
    assert_eq!(query(&server, json!({ "meta.duration_ms": { "$gte": 1000 } }))?, vec!["take5.wav"]);
    assert_eq!(query(&server, json!({ "$text": "brubeck" }))?, vec!["take5.wav"]);
    assert_eq!(query(&server, json!({ "$text": "glacier lagoon" }))?, vec!["notes.md"]);
    assert_eq!(query(&server, json!({ "$text": "take5" }))?, vec!["take5.wav"], "names are searchable");
    let sql = server.request(Method::POST, "/sql", Some(&json!({ "sql": "SELECT name, meta.title FROM media WHERE meta.artist = 'Dave Brubeck Quartet'" })), &[])?;
    assert_eq!(sql.body["rows"], json!([["take5.wav", "Take Five"]]), "{}", sql.body);
    let gql = json!({
        "query": "query Find($filter: JSON) { documents(tessellation: \"media\", filter: $filter) { total documents { field(path: \"meta.title\") } } }",
        "variables": { "filter": { "meta.title": "Release notes" } }
    });
    let res = server.request(Method::POST, "/graphql", Some(&gql), &[])?;
    assert_eq!(res.body["data"]["documents"]["total"], 1, "{}", res.body);

    // Extracted metadata can't be overwritten through the document routes.
    let res = server.request(Method::PATCH, &format!("/media/{}", wav_id), Some(&json!({ "meta": { "title": "forged" } })), &[])?;
    assert_eq!(res.body["meta"]["title"], "Take Five");

    // A new version is extracted again.
    upload(&server, "music/take5.wav", &tagged_wav("Blue Rondo", "Dave Brubeck Quartet"))?;
    let mut title = Value::Null;
    for _ in 0..100 {
        let doc = server.get_doc("media", &wav_id)?.unwrap();
        if doc["extract"]["status"] == "done" {
            title = doc["meta"]["title"].clone();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert_eq!(title, "Blue Rondo");

    let res = server.request(Method::POST, "/media/_reextract", None, &[])?;
    assert_eq!(res.status, StatusCode::ACCEPTED);
    assert_eq!(res.body["queued"], 2);
    Ok(())
}

fn wait_until(seconds: u64, what: &str, mut check: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while !check()? {
        if std::time::Instant::now() > deadline {
            anyhow::bail!("timed out waiting for {}", what);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Ok(())
}

fn pair(ports: [u16; 2], i: usize, ram_mb: u64) -> hexdb_tests::TestOptions {
    hexdb_tests::TestOptions {
        lattice: Some(format!("files-{}", ports[0])),
        discovery_port: Some(ports[i]),
        peers: vec![format!("127.0.0.1:{}", ports[1 - i])],
        ram_mb: Some(ram_mb),
        discovery_interval_seconds: Some(1),
        disk_mb: Some(4096),
        ..Default::default()
    }
}

#[test]
fn files_replicate_stream_through_replicas_and_repair_from_peers() -> Result<()> {
    let ports = [TestServer::free_port()?, TestServer::free_port()?];
    let overseer = TestServer::start_with(pair(ports, 0, 4096))?;
    let mut replica = TestServer::start_with(pair(ports, 1, 1024))?;
    let res = overseer.request(Method::POST, "/tessellations", Some(&json!({ "name": "media", "kind": "files" })), &[])?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let content = noise(3_000_000, 7);
    let doc = upload(&overseer, "a/one.bin", &content)?;
    let id = doc["id"].as_str().unwrap().to_string();

    // The document and the content reach the replica.
    wait_until(60, "the file on the replica", || Ok(replica.get_doc("media", &id)?.is_some()))?;
    let info = replica.request(Method::GET, "/tessellations/media", None, &[])?;
    assert_eq!(info.body["kind"], "files", "{}", info.body);
    // Its chunks plus the manifest, copied in the background.
    let expected = doc["content"]["chunks"].as_u64().unwrap() + 1;
    wait_until(60, "the chunks on the replica", || {
        let status = replica.request(Method::GET, "/status", None, &[])?;
        Ok(status.body["storage"]["files"]["chunks"].as_u64().unwrap_or(0) >= expected)
    })?;
    let status = replica.request(Method::GET, "/status", None, &[])?;
    assert_eq!(status.body["storage"]["files"]["fetch_failures"], 0, "{}", status.body["storage"]["files"]);
    assert_eq!(download(&replica, &format!("/media/{}/content", id))?, content);

    // An upload larger than the request limit, sent to the replica, is streamed to the Overseer.
    let big = noise(40 * 1024 * 1024, 8);
    let res = replica.send_bytes(Method::PUT, "/media/_files/big.bin", big.clone(), &[])?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert!(res.headers.get("x-hexdb-forwarded-to").is_some());
    assert_eq!(res.body["sha256"], sha256(&big));
    let big_id = res.body["id"].as_str().unwrap().to_string();
    assert_eq!(sha256(&download(&overseer, &format!("/media/{}/content", big_id))?), sha256(&big));
    wait_until(60, "the big file's document on the replica", || Ok(replica.get_doc("media", &big_id)?.is_some()))?;
    assert_eq!(sha256(&download(&replica, &format!("/media/{}/content", big_id))?), sha256(&big));

    // A replica that lost three vertices gets the content back from the Overseer.
    replica.stop()?;
    for v in [0, 2, 3] {
        std::fs::remove_dir_all(replica.data_dir().join("blobs").join(format!("vertex-{}", v)))?;
    }
    replica.launch()?;
    wait_until(60, "the replica to rejoin", || {
        Ok(replica.request(Method::GET, "/status", None, &[])?.body["replication"]["state"] == "streaming")
    })?;
    assert_eq!(download(&replica, &format!("/media/{}/content", id))?, content);
    let status = replica.request(Method::GET, "/status", None, &[])?;
    assert!(status.body["storage"]["files"]["fetched_from_peers"].as_u64().unwrap() >= 1, "{}", status.body["storage"]["files"]);
    Ok(())
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[test]
fn backups_carry_file_content() -> Result<()> {
    let source = files_server()?;
    let a = noise(2_500_000, 9);
    let b = b"small text file".to_vec();
    let a_id = upload(&source, "keep/a.bin", &a)?["id"].as_str().unwrap().to_string();
    upload(&source, "keep/b.txt", &b)?;
    let res = source.request(Method::POST, "/backup", Some(&json!({ "name": "with-files" })), &[])?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let path = res.body["path"].as_str().unwrap().to_string();
    upload(&source, "later/c.bin", b"after the backup")?;

    let mut restored = TestServer::start()?;
    restored.stop()?;
    std::fs::remove_dir_all(restored.data_dir())?;
    copy_dir(std::path::Path::new(&path), &restored.data_dir())?;
    restored.launch()?;
    assert_eq!(download(&restored, &format!("/media/{}/content", a_id))?, a);
    let (_, _, text) = restored.get_bytes("/media/_files/keep/b.txt", &[])?;
    assert_eq!(text, b);
    let (status, _, _) = restored.get_bytes("/media/_files/later/c.bin", &[])?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    upload(&restored, "new/d.txt", b"the restored hex takes uploads")?;
    Ok(())
}

#[test]
fn resumable_uploads() -> Result<()> {
    let server = files_server()?;
    let content = noise(7_000_000, 10);
    let res = server.request(Method::POST, "/media/_uploads", Some(&json!({ "path": "video/raw.mov", "size": content.len() })), &[])?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let id = res.body["id"].as_str().unwrap().to_string();
    assert_eq!(res.body["offset"], 0);
    assert_eq!(res.body["path"], "/video/raw.mov");

    let part = |offset: usize, bytes: &[u8]| server.send_bytes(Method::PATCH, &format!("/media/_uploads/{}?offset={}", id, offset), bytes.to_vec(), &[]);
    let res = part(0, &content[..3_000_000])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["offset"], 3_000_000);

    // A part at the wrong offset is refused; the status says where to resume.
    let res = part(1_000, &content[1_000..2_000])?;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let status = server.request(Method::GET, &format!("/media/_uploads/{}", id), None, &[])?;
    assert_eq!(status.body["offset"], 3_000_000);

    // Garbage collection keeps an upload's chunks while it's in progress.
    server.request(Method::POST, "/compact", None, &[])?;

    // Completing early is refused; then the rest, with the header form of the offset.
    let res = server.request(Method::POST, &format!("/media/_uploads/{}/complete", id), None, &[])?;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let res = server.send_bytes(Method::PATCH, &format!("/media/_uploads/{}", id), content[3_000_000..].to_vec(), &[("upload-offset", "3000000")])?;
    assert_eq!(res.body["offset"], content.len());
    let res = server.request(Method::POST, &format!("/media/_uploads/{}/complete", id), None, &[])?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["sha256"], sha256(&content));
    assert_eq!(res.body["size"], content.len());
    let file_id = res.body["id"].as_str().unwrap().to_string();
    assert_eq!(download(&server, &format!("/media/{}/content", file_id))?, content);
    let res = server.request(Method::GET, &format!("/media/_uploads/{}", id), None, &[])?;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "a completed session is gone");

    // An abandoned upload goes away.
    let res = server.request(Method::POST, "/media/_uploads", Some(&json!({ "path": "tmp/x.bin" })), &[])?;
    let other = res.body["id"].as_str().unwrap().to_string();
    server.send_bytes(Method::PATCH, &format!("/media/_uploads/{}?offset=0", other), b"partial".to_vec(), &[])?;
    let res = server.request(Method::DELETE, &format!("/media/_uploads/{}", other), None, &[])?;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert_eq!(server.request(Method::GET, &format!("/media/_uploads/{}", other), None, &[])?.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[test]
fn images_get_thumbnails() -> Result<()> {
    let server = files_server()?;
    let mut photo = Vec::new();
    image::RgbImage::from_fn(1600, 900, |x, y| image::Rgb([(x / 7) as u8, (y / 4) as u8, 90]))
        .write_to(&mut std::io::Cursor::new(&mut photo), image::ImageFormat::Jpeg)?;
    let id = upload(&server, "photos/wide.jpg", &photo)?["id"].as_str().unwrap().to_string();
    let doc = extracted(&server, &id)?;
    assert_eq!(doc["thumbnail"]["width"], 320, "{}", doc);
    assert_eq!(doc["thumbnail"]["height"], 180);
    assert_eq!(doc["meta"]["width"], 1600);
    let (status, headers, bytes) = server.get_bytes(&format!("/media/{}/thumbnail", id), &[])?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/jpeg");
    let thumb = image::load_from_memory(&bytes)?;
    assert_eq!((thumb.width(), thumb.height()), (320, 180));

    // Thumbnails survive garbage collection; files without one say so.
    server.request(Method::POST, "/compact", None, &[])?;
    assert_eq!(server.get_bytes(&format!("/media/{}/thumbnail", id), &[])?.0, StatusCode::OK);
    let text = upload(&server, "notes/a.txt", b"no picture here")?["id"].as_str().unwrap().to_string();
    extracted(&server, &text)?;
    assert_eq!(server.get_bytes(&format!("/media/{}/thumbnail", text), &[])?.0, StatusCode::NOT_FOUND);
    Ok(())
}
