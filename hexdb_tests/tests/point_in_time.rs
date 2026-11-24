//! Point-in-time restore: the Overseer ships to an S3-compatible store, and a
//! restore rebuilds the lattice as it was at a chosen moment, documents and
//! file content included. Runs when HEXDB_TEST_S3_ENDPOINT names a store (with
//! HEXDB_TEST_S3_KEY and HEXDB_TEST_S3_SECRET): `docker run -p 9000:9000
//! rustfs/rustfs` locally, a service in CI.

use anyhow::{bail, Result};
use hexdb_core::objstore::{ObjectStore, StoreConfig};
use hexdb_tests::{TestServer, TEST_ENCRYPTION_KEY};
use reqwest::Method;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn store_config() -> Option<StoreConfig> {
    let endpoint = std::env::var("HEXDB_TEST_S3_ENDPOINT").ok()?;
    Some(StoreConfig {
        endpoint,
        bucket: "hexdb-pitr-tests".into(),
        region: "us-east-1".into(),
        access_key_id: std::env::var("HEXDB_TEST_S3_KEY").unwrap_or_else(|_| "rustfsadmin".into()),
        secret_access_key: std::env::var("HEXDB_TEST_S3_SECRET").unwrap_or_else(|_| "rustfsadmin".into()),
    })
}

fn status(server: &TestServer) -> Result<Value> {
    Ok(server.request(Method::GET, "/backups/continuous", None, &[])?.body)
}

/// Wait until shipping has moved past `after` (segments shipped through a higher sequence).
fn wait_shipped(server: &TestServer, after: u64) -> Result<u64> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let s = status(server)?;
        let through = s["shipped_through"].as_u64().unwrap_or(0);
        if through > after && s["last_base"].is_object() {
            return Ok(through);
        }
        if Instant::now() > deadline {
            bail!("shipping didn't move past {}: {}", after, s);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn ids(server: &TestServer) -> Result<Vec<i64>> {
    let res = server.request(Method::POST, "/orders/_query", Some(&json!({ "limit": 1000, "sort": [{ "field": "n" }] })), &[])?;
    Ok(res.body["documents"].as_array().cloned().unwrap_or_default().iter().filter_map(|d| d["n"].as_i64()).collect())
}

/// Restore into a fresh server's data folder and start it on that.
fn restored(config: &StoreConfig, prefix: &str, until: Option<i64>) -> Result<TestServer> {
    let mut server = TestServer::start()?;
    server.stop()?;
    std::fs::remove_dir_all(server.data_dir())?;
    let keys = hexdb_core::crypt::KeyRing::new(&hexdb_core::crypt::decode_encryption_key(TEST_ENCRYPTION_KEY)?, &[]);
    let report = tokio::runtime::Runtime::new()?.block_on(hexdb_core::restore::restore(config.clone(), prefix, until, &server.data_dir(), &keys))?;
    assert!(report.segments > 0, "{:?}", report);
    server.launch()?;
    Ok(server)
}

#[test]
fn ships_and_restores_to_a_moment() -> Result<()> {
    let Some(config) = store_config() else { return Ok(()) };
    tokio::runtime::Runtime::new()?.block_on(ObjectStore::new(config.clone())?.ensure_bucket())?;
    let prefix = format!("t-{}", ulid::Ulid::generate().to_string().to_ascii_lowercase());

    let server = TestServer::start()?;
    let changes = json!({
        "backup.continuous.enabled": true,
        "backup.continuous.endpoint": config.endpoint,
        "backup.continuous.bucket": config.bucket,
        "backup.continuous.prefix": prefix,
        "backup.continuous.access_key_id": config.access_key_id,
        "backup.continuous.secret_access_key": config.secret_access_key,
        "backup.continuous.max_lag_seconds": 10,
    });
    let res = server.request(Method::PUT, "/config", Some(&json!({ "changes": changes, "comment": "point-in-time test" })), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    assert!(server.request(Method::POST, "/tessellations", Some(&json!({ "name": "media", "kind": "files" })), &[])?.status.is_success());

    // A base first (shipping just started), then a first batch with a file.
    let through = wait_shipped(&server, 0)?;
    for n in 0..5 {
        server.insert("orders", &json!({ "n": n }))?;
    }
    let first: Vec<u8> = (0..400_000u32).map(|i| (i % 253) as u8).collect();
    assert!(server.send_bytes(Method::PUT, "/media/_files/first.bin", first.clone(), &[])?.status.is_success());
    let through = wait_shipped(&server, through)?;
    std::thread::sleep(Duration::from_millis(1500));
    let moment = jiff::Timestamp::now().as_millisecond();
    std::thread::sleep(Duration::from_millis(1500));

    // A second batch after the moment.
    for n in 5..9 {
        server.insert("orders", &json!({ "n": n }))?;
    }
    let second = vec![7u8; 300_000];
    assert!(server.send_bytes(Method::PUT, "/media/_files/second.bin", second.clone(), &[])?.status.is_success());
    wait_shipped(&server, through)?;
    assert!(status(&server)?["last_error"].is_null(), "{}", status(&server)?);

    // At the moment: the first batch and its file, nothing after.
    let then = restored(&config, &prefix, Some(moment))?;
    assert_eq!(ids(&then)?, vec![0, 1, 2, 3, 4]);
    let (code, _, bytes) = then.get_bytes("/media/_files/first.bin", &[])?;
    assert_eq!(code.as_u16(), 200);
    assert!(bytes == first, "the first file, byte for byte");
    assert_eq!(then.get_bytes("/media/_files/second.bin", &[])?.0.as_u16(), 404);

    // The newest moment: everything.
    let now = restored(&config, &prefix, None)?;
    assert_eq!(ids(&now)?, (0..9).collect::<Vec<_>>());
    let (code, _, bytes) = now.get_bytes("/media/_files/second.bin", &[])?;
    assert_eq!(code.as_u16(), 200);
    assert!(bytes == second, "the second file, byte for byte");
    Ok(())
}
