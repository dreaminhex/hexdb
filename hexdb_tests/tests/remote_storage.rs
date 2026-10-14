//! File content on remote backends, against real S3- and Azure-compatible
//! servers. Skipped unless both are given:
//!
//!   HEXDB_TEST_S3="<endpoint>|<bucket>|<access key>|<secret key>"
//!   HEXDB_TEST_AZURE="<endpoint>|<account>|<container>|<account key>"
//!
//! e.g. SeaweedFS (`weed server -s3`) and Azurite (`azurite-blob --loose`).

use anyhow::{bail, Result};
use hexdb_tests::{TestOptions, TestServer};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn backend(var: &str) -> Option<Vec<String>> {
    let value = std::env::var(var).ok()?;
    let parts: Vec<String> = value.split('|').map(String::from).collect();
    (parts.len() == 4).then_some(parts)
}

fn wait_until(timeout: Duration, what: &str, mut check: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if check()? {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {}", what);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn remote(server: &TestServer) -> Result<Value> {
    Ok(server.request(Method::GET, "/status", None, &[])?.body["storage"]["files"]["remote"].clone())
}

fn set(server: &TestServer, changes: Value) -> Result<()> {
    let res = server.request(Method::PUT, "/config", Some(&json!({ "changes": changes })), &[])?;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    Ok(())
}

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

fn download(server: &TestServer, id: &str) -> Result<Vec<u8>> {
    let (status, _, bytes) = server.get_bytes(&format!("/media/{}/content", id), &[])?;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&bytes[..bytes.len().min(300)]));
    Ok(bytes)
}

#[test]
fn content_moves_to_s3_then_azure_then_back() -> Result<()> {
    let (Some(s3), Some(azure)) = (backend("HEXDB_TEST_S3"), backend("HEXDB_TEST_AZURE")) else {
        eprintln!("skipped: set HEXDB_TEST_S3 and HEXDB_TEST_AZURE to run it");
        return Ok(());
    };
    let mut server = TestServer::start_with(TestOptions { extra_toml: "\n[files]\nremote_seal_seconds = 1\n".into(), ..Default::default() })?;
    let res = server.request(Method::POST, "/tessellations", Some(&json!({ "name": "media", "kind": "files" })), &[])?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let content = noise(3_000_000, 7);
    let res = server.send_bytes(Method::PUT, "/media/_files/raw/take.bin", content.clone(), &[])?;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let id = res.body["id"].as_str().unwrap().to_string();
    let prefix = format!("hexdb-test-{}", ulid::Ulid::new().to_string().to_lowercase());

    // S3: everything is uploaded and, with no local budget, read back from there.
    set(&server, json!({
        "file_storage.backend": "s3", "file_storage.prefix": prefix,
        "file_storage.s3.endpoint": s3[0], "file_storage.s3.bucket": s3[1], "file_storage.s3.access_key_id": s3[2],
        "file_storage.s3.secret_access_key": s3[3], "file_storage.local_cache_gb": 0,
    }))?;
    let test = server.request(Method::POST, "/config/storage-test", None, &[])?;
    assert_eq!(test.body["ok"], true, "{}", test.body);
    wait_until(Duration::from_secs(90), "packs only on S3", || {
        let r = remote(&server)?;
        Ok(r["backend"] == "s3" && r["remote_only_packs"].as_u64().unwrap_or(0) >= 6 && r["pending_packs"] == 0)
    })?;
    assert_eq!(download(&server, &id)?, content, "read back (possibly from the memory cache)");

    // A restart rebuilds the index from the local index files; the memory cache
    // starts empty, so the content comes from S3.
    server.restart()?;
    assert_eq!(download(&server, &id)?, content, "after a restart");
    assert!(remote(&server)?["bytes_downloaded"].as_u64().unwrap() > 0, "{}", remote(&server)?);

    // Azure: packs are moved off S3, uploaded to Azure, and read from there.
    set(&server, json!({
        "file_storage.backend": "azure", "file_storage.azure.endpoint": azure[0], "file_storage.azure.account": azure[1],
        "file_storage.azure.container": azure[2], "file_storage.azure.account_key": azure[3],
    }))?;
    wait_until(Duration::from_secs(120), "packs moved to Azure", || {
        let r = remote(&server)?;
        Ok(r["backend"] == "azure" && r["moving_packs"] == 0 && r["pending_packs"] == 0 && r["remote_only_packs"].as_u64().unwrap_or(0) >= 6)
    })?;
    assert_eq!(download(&server, &id)?, content, "read back from Azure");

    // Back to local disks: everything comes home.
    set(&server, json!({ "file_storage.backend": "local" }))?;
    wait_until(Duration::from_secs(120), "packs back on local disks", || {
        let r = remote(&server)?;
        Ok(r["backend"].is_null() && r["remote_only_packs"] == 0)
    })?;
    assert_eq!(download(&server, &id)?, content, "read locally again");
    let scrub = server.request(Method::POST, "/scrub", None, &[])?;
    assert_eq!(scrub.body["unrecoverable"], 0, "{}", scrub.body);

    // A wrong key fails the storage test with the provider's error.
    set(&server, json!({ "file_storage.backend": "s3", "file_storage.s3.secret_access_key": "wrong-secret" }))?;
    let test = server.request(Method::POST, "/config/storage-test", None, &[])?;
    assert_eq!(test.body["ok"], false, "{}", test.body);
    assert!(test.body["error"].as_str().unwrap().contains("403"), "{}", test.body);
    assert!(!test.body.to_string().contains("wrong-secret"));
    Ok(())
}
