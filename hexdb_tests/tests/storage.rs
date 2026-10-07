//! Storage end-to-end tests: documents written through the API must read back
//! correctly, including across graceful restarts, crashes, and flushes to SSTables.
//!
//! Tests marked `#[ignore]` describe known Phase 2 bugs (see TODO.md). Run them with:
//!
//!     cargo test -p hexdb_tests -- --ignored
//!
//! Remove the `#[ignore]` once the bug is fixed.

use anyhow::Result;
use hexdb_tests::{field, TestServer};
use serde_json::json;
use std::{fs::OpenOptions, io::Write, thread, time::Duration};

// ---------------------------------------------------------------------------
// Behavior that works today
// ---------------------------------------------------------------------------

#[test]
fn server_reports_health() -> Result<()> {
    let server = TestServer::start()?;
    let health: serde_json::Value = server.get("/health")?.error_for_status()?.json()?;
    assert_eq!(health["status"], "ok");
    Ok(())
}

#[test]
fn insert_then_read_back() -> Result<()> {
    let server = TestServer::start()?;
    let id = server.insert("articles", &json!({ "title": "Quantum Tessellation", "views": 445, "published": true }))?;

    assert_eq!(server.count("articles")?, 1);
    let doc = server.get_doc("articles", &id)?.expect("document should exist");
    assert_eq!(field(&doc, "title"), Some(json!("Quantum Tessellation")));
    assert_eq!(field(&doc, "views"), Some(json!(445)));
    assert_eq!(field(&doc, "published"), Some(json!(true)));
    Ok(())
}

#[test]
fn patch_updates_only_given_fields() -> Result<()> {
    let server = TestServer::start()?;
    let id = server.insert("articles", &json!({ "title": "Original title", "views": 1 }))?;

    server.patch("articles", &json!({ "id": id, "views": 2 }))?;

    let doc = server.get_doc("articles", &id)?.expect("document should exist");
    assert_eq!(field(&doc, "title"), Some(json!("Original title")));
    assert_eq!(field(&doc, "views"), Some(json!(2)));
    Ok(())
}

#[test]
fn delete_removes_document() -> Result<()> {
    let server = TestServer::start()?;
    let id = server.insert("articles", &json!({ "title": "Doomed" }))?;

    server.delete("articles", &id)?;

    assert!(server.get_doc("articles", &id)?.is_none());
    assert_eq!(server.count("articles")?, 0);
    Ok(())
}

#[test]
fn documents_survive_graceful_restart() -> Result<()> {
    let mut server = TestServer::start()?;
    let id = server.insert("articles", &json!({ "title": "Persistent" }))?;
    server.insert("articles", &json!({ "title": "Also persistent" }))?;

    server.restart()?;

    assert_eq!(server.count("articles")?, 2);
    let doc = server.get_doc("articles", &id)?.expect("document should survive restart");
    assert_eq!(field(&doc, "title"), Some(json!("Persistent")));
    Ok(())
}

#[test]
fn documents_survive_crash_once_writes_settle() -> Result<()> {
    let mut server = TestServer::start()?;
    let id = server.insert("articles", &json!({ "title": "Crash survivor" }))?;
    // Writes are acknowledged before they reach the WAL (see the durability
    // test below), so give the WAL writer a moment before pulling the plug.
    thread::sleep(Duration::from_millis(500));

    server.crash_and_restart()?;

    assert!(server.get_doc("articles", &id)?.is_some());
    Ok(())
}

#[test]
fn delete_survives_restart_without_flush() -> Result<()> {
    let mut server = TestServer::start()?;
    let id = server.insert("articles", &json!({ "title": "Doomed" }))?;
    server.delete("articles", &id)?;

    server.restart()?;

    assert!(server.get_doc("articles", &id)?.is_none());
    Ok(())
}

#[test]
fn documents_survive_flush_and_restart() -> Result<()> {
    let mut server = TestServer::start()?;
    let ids: Vec<String> = (0..5)
        .map(|i| server.insert("articles", &json!({ "title": format!("Doc {}", i) })))
        .collect::<Result<_>>()?;

    server.flush()?;
    server.restart()?;

    assert_eq!(server.count("articles")?, 5);
    for id in &ids {
        assert!(server.get_doc("articles", id)?.is_some(), "missing {}", id);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Known Phase 2 bugs
// ---------------------------------------------------------------------------

#[test]
#[ignore = "Phase 2: deletes leave no tombstone in SSTables, so flushed documents come back"]
fn delete_after_flush_survives_restart() -> Result<()> {
    let mut server = TestServer::start()?;
    let id = server.insert("articles", &json!({ "title": "Doomed" }))?;
    server.flush()?;
    server.delete("articles", &id)?;

    server.restart()?;

    assert!(server.get_doc("articles", &id)?.is_none(), "deleted document came back");
    assert_eq!(server.count("articles")?, 0);
    Ok(())
}

#[test]
#[ignore = "Phase 2: startup replays the WAL before loading SSTables, so older versions win"]
fn update_after_flush_survives_restart() -> Result<()> {
    let mut server = TestServer::start()?;
    let id = server.insert("articles", &json!({ "title": "Draft", "views": 1 }))?;
    server.flush()?;
    server.patch("articles", &json!({ "id": id, "views": 2 }))?;

    server.restart()?;

    let doc = server.get_doc("articles", &id)?.expect("document should exist");
    assert_eq!(field(&doc, "views"), Some(json!(2)), "an older version won after restart");
    Ok(())
}

#[test]
#[ignore = "Phase 2: strings that happen to be valid base64 are stored as binary"]
fn strings_keep_their_type() -> Result<()> {
    let server = TestServer::start()?;
    let id = server.insert("articles", &json!({ "name": "Test", "code": "abcd", "title": "hello" }))?;

    let doc = server.get_doc("articles", &id)?.expect("document should exist");
    assert_eq!(field(&doc, "name"), Some(json!("Test")));
    assert_eq!(field(&doc, "code"), Some(json!("abcd")));
    assert_eq!(field(&doc, "title"), Some(json!("hello")));
    Ok(())
}

#[test]
#[ignore = "Phase 2: arrays and objects are flattened to JSON strings"]
fn arrays_and_objects_round_trip() -> Result<()> {
    let server = TestServer::start()?;
    let id = server.insert(
        "articles",
        &json!({ "tags": ["hexdb", "rust", "ai"], "author": { "name": "Ada", "id": 7 } }),
    )?;

    let doc = server.get_doc("articles", &id)?.expect("document should exist");
    assert_eq!(field(&doc, "tags"), Some(json!(["hexdb", "rust", "ai"])));
    assert_eq!(field(&doc, "author"), Some(json!({ "name": "Ada", "id": 7 })));
    Ok(())
}

#[test]
#[ignore = "Phase 2: a partial record at the end of the WAL stops the server from starting"]
fn torn_wal_tail_does_not_block_startup() -> Result<()> {
    let mut server = TestServer::start()?;
    let id = server.insert("articles", &json!({ "title": "Before the tear" }))?;
    server.stop()?;

    // Simulate a crash mid-write: a length prefix promising more bytes than follow.
    let mut wal = OpenOptions::new().append(true).open(server.wal_path())?;
    wal.write_all(&[0x00, 0x00, 0x10, 0x00, b'a', b'b', b'c'])?;
    drop(wal);

    server.launch()?;

    assert!(server.get_doc("articles", &id)?.is_some());
    Ok(())
}

// ---------------------------------------------------------------------------
// Known Phase 2 bugs (continued)
// ---------------------------------------------------------------------------

/// Concurrent writes followed immediately by a graceful restart must all survive.
///
/// About 10-15% of 400 acknowledged writes are lost: they are still queued for
/// the WAL writer when the process exits.
#[test]
#[ignore = "Phase 2: graceful shutdown doesn't drain queued WAL writes, so acknowledged writes are lost"]
fn concurrent_writes_survive_immediate_graceful_restart() -> Result<()> {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 50;

    let mut server = TestServer::start()?;
    thread::scope(|s| -> Result<()> {
        let workers: Vec<_> = (0..THREADS)
            .map(|t| {
                let server = &server;
                s.spawn(move || -> Result<()> {
                    for i in 0..PER_THREAD {
                        server.post_document("articles", &json!({ "title": format!("Doc {}-{}", t, i) }))?;
                    }
                    Ok(())
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("worker panicked")?;
        }
        Ok(())
    })?;

    assert_eq!(server.count("articles")?, THREADS * PER_THREAD, "documents missing before restart");

    server.restart()?;

    assert_eq!(server.count("articles")?, THREADS * PER_THREAD);
    Ok(())
}
