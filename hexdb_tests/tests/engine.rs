//! In-process storage engine tests. These open `HexDBEngine` directly in a
//! temporary directory to exercise behavior that is hard to trigger over HTTP:
//! memory eviction, compaction, vertex repair, and recovery edge cases.

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use hexdb_core::{EngineError, HexConfig, HexDBEngine, HexIdentity};
use serde_json::json;
use std::{path::Path, time::Duration};
use tempfile::TempDir;
use ulid::Ulid;

const KEY: [u8; 32] = [7u8; 32];

fn config(dir: &Path, ram_mb: u64) -> HexConfig {
    let mut config = HexConfig::default();
    config.storage.path = dir.join("data").to_string_lossy().into_owned();
    config.storage.encryption_key = format!("base64:{}", STANDARD.encode(KEY));
    config.memory.ram_mb = ram_mb;
    config
}

async fn open(dir: &Path, ram_mb: u64, key: &[u8]) -> Result<HexDBEngine> {
    let identity = HexIdentity { id: Ulid::new(), name: "Test".into(), hex_type: "Overseer".into() };
    let key: [u8; 32] = key.try_into().expect("32-byte key");
    let keys = std::sync::Arc::new(hexdb_core::KeyRing::new(&key, &[]));
    HexDBEngine::open(config(dir, ram_mb), identity, keys).await
}

#[tokio::test]
async fn evicts_cached_documents_over_budget_and_reads_them_from_disk() -> Result<()> {
    let dir = TempDir::new()?;
    let engine = open(dir.path(), 1, &KEY).await?;
    let filler = "x".repeat(10_000);

    let mut ids = Vec::new();
    for i in 0..300 {
        ids.push(engine.insert_json("big", json!({ "n": i, "filler": filler }), None).await?.id);
    }
    engine.flush().await?;
    engine.enforce_memory_budget().await;

    let stats = engine.stats().await;
    assert!(
        stats.memory_bytes <= stats.ram_budget_bytes,
        "memory {} exceeds budget {}",
        stats.memory_bytes,
        stats.ram_budget_bytes
    );
    assert_eq!(engine.count_documents("big").await?, 300);
    assert!(engine.tessellation_stats("big").await.documents_on_disk_only > 0);

    for (i, id) in ids.iter().enumerate().step_by(37) {
        let doc = engine.get_document("big", &id.to_string()).await?.expect("evicted document readable from disk");
        assert_eq!(doc.data_json()["n"], json!(i));
    }

    engine.shutdown().await
}

#[tokio::test]
async fn compaction_drops_deleted_and_expired_documents() -> Result<()> {
    let dir = TempDir::new()?;
    let engine = open(dir.path(), 64, &KEY).await?;

    let deleted = engine.insert_json("t", json!({ "name": "deleted" }), None).await?.id;
    let expiring = engine
        .insert_json("t", json!({ "name": "expiring" }), Some(chrono::Utc::now().timestamp_millis() + 200))
        .await?
        .id;
    let kept = engine.insert_json("t", json!({ "name": "kept" }), None).await?.id;
    engine.flush().await?;

    assert!(engine.delete_document("t", &deleted.to_string(), None).await?.value);
    engine.flush().await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let stats = engine.compact().await?;
    assert_eq!(stats.files_merged, 2);
    assert_eq!(stats.entries_kept, 1, "only the live document should remain on disk");
    assert_eq!(engine.stats().await.sst_files, 1);
    assert_eq!(engine.count_documents("t").await?, 1);

    engine.shutdown().await?;
    drop(engine);

    let engine = open(dir.path(), 64, &KEY).await?;
    assert!(engine.get_document("t", &deleted.to_string()).await?.is_none());
    assert!(engine.get_document("t", &expiring.to_string()).await?.is_none());
    assert!(engine.get_document("t", &kept.to_string()).await?.is_some());
    engine.shutdown().await
}

#[tokio::test]
async fn sequence_numbers_never_go_backwards() -> Result<()> {
    let dir = TempDir::new()?;
    let engine = open(dir.path(), 64, &KEY).await?;
    let id = engine.insert_json("t", json!({ "a": 1 }), None).await?.id;
    engine.delete_document("t", &id.to_string(), None).await?;
    engine.flush().await?;
    engine.compact().await?; // drops the tombstone and the only document
    let before = engine.stats().await.next_seq;
    engine.shutdown().await?;
    drop(engine);

    let engine = open(dir.path(), 64, &KEY).await?;
    assert!(engine.stats().await.next_seq >= before, "sequence numbers were reused after restart");
    engine.shutdown().await
}

#[tokio::test]
async fn corrupt_vertices_are_repaired() -> Result<()> {
    let dir = TempDir::new()?;
    let engine = open(dir.path(), 64, &KEY).await?;
    let doc = engine.insert_json("t", json!({ "title": "fragile", "n": 42 }), None).await?;

    assert!(engine.corrupt_shard_for_testing("t", doc.id, 1).await);
    assert!(engine.corrupt_shard_for_testing("t", doc.id, 4).await);

    let read = engine.get_document("t", &doc.id.to_string()).await?.expect("readable despite corruption");
    assert_eq!(read.data_json(), doc.data_json());

    let report = engine.check_vertices().await;
    assert_eq!(report.repaired_shards, 2);
    assert_eq!(engine.check_vertices().await.corrupt_shards, 0);

    let vertices = engine.stats().await.vertices;
    assert_eq!(vertices[1].repaired, 1);
    assert_eq!(vertices[4].repaired, 1);
    engine.shutdown().await
}

#[tokio::test]
async fn tessellation_names_are_case_insensitively_unique() -> Result<()> {
    let dir = TempDir::new()?;
    let engine = open(dir.path(), 64, &KEY).await?;
    assert!(engine.create_tessellation("Articles", "user")?);
    assert!(!engine.create_tessellation("Articles", "user")?);

    let err = engine.insert_json("articles", json!({ "a": 1 }), None).await.unwrap_err();
    assert!(matches!(err.downcast_ref::<EngineError>(), Some(EngineError::Conflict(_))), "{:#}", err);
    engine.shutdown().await
}

#[tokio::test]
async fn empty_tessellations_persist() -> Result<()> {
    let dir = TempDir::new()?;
    let engine = open(dir.path(), 64, &KEY).await?;
    engine.create_tessellation("empty", "user")?;
    engine.shutdown().await?;
    drop(engine);

    let engine = open(dir.path(), 64, &KEY).await?;
    assert!(engine.tessellation_exists("empty"));
    engine.shutdown().await
}

#[tokio::test]
async fn refuses_to_start_with_the_wrong_key() -> Result<()> {
    let dir = TempDir::new()?;
    let engine = open(dir.path(), 64, &KEY).await?;
    engine.insert_json("t", json!({ "a": 1 }), None).await?;
    drop(engine); // simulated crash: the write is only in the WAL

    let err = open(dir.path(), 64, &[9u8; 32]).await.err().expect("must refuse to start");
    assert!(format!("{:#}", err).contains("encryption_key"), "{:#}", err);

    // The WAL is untouched, so the right key still recovers the write.
    let engine = open(dir.path(), 64, &KEY).await?;
    assert_eq!(engine.count_documents("t").await?, 1);
    engine.shutdown().await
}

#[tokio::test]
async fn legacy_users_and_roles_are_migrated() -> Result<()> {
    use hexdb_core::users;

    let dir = TempDir::new()?;
    let engine = open(dir.path(), 64, &KEY).await?;
    let hash = hexdb_core::create_hash("legacy password");

    // The layout written by earlier builds: one document holding an array.
    engine.create_tessellation("roles", "system")?;
    engine.create_tessellation("users", "system")?;
    engine
        .insert_json("roles", json!({ "roles": [
            { "name": "admin", "description": "Full system access." },
            { "name": "reader", "description": "Read-only." }
        ] }), None)
        .await?;
    engine
        .insert_json("users", json!({ "users": [{
            "login": "legacyadmin", "password": hash, "email_address": "old@hexdb.ai",
            "created": 1, "roles": [{ "name": "admin", "permissions": ["*"] }]
        }] }), None)
        .await?;

    let mut config = HexConfig::default();
    config.security.admin_login = "newadmin".into();
    users::bootstrap(&engine, &config.security).await?;

    let all = users::list_users(&engine).await?;
    assert_eq!(all.len(), 1, "the migrated admin exists, so no new admin is created");
    assert_eq!(all[0].login, "legacyadmin");
    assert_eq!(engine.count_documents("users").await?, 1, "the legacy array document is removed");

    let roles: Vec<String> = users::list_roles(&engine).await?.into_iter().map(|r| r.name).collect();
    assert_eq!(roles.len(), 6, "the built-in roles: {:?}", roles);
    engine.shutdown().await
}
