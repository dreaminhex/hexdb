//! Storage end-to-end tests: documents written through the API must read back
//! correctly, including across graceful restarts, crashes, and flushes to SSTables.
//!
//! Tests marked `#[ignore]` describe known bugs (see TODO.md). Run them with:
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
fn acknowledged_writes_survive_crash() -> Result<()> {
    let mut server = TestServer::start()?;
    let id = server.insert("articles", &json!({ "title": "Crash survivor" }))?;

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
// Phase 2 regressions (these were bugs before Phase 2)
// ---------------------------------------------------------------------------

#[test]
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
fn torn_wal_tail_does_not_block_startup() -> Result<()> {
    let mut server = TestServer::start()?;
    let id = server.insert("articles", &json!({ "title": "Before the tear" }))?;
    // Crash so the write exists only in the WAL.
    server.kill();

    // Simulate a crash mid-write: a length prefix promising more bytes than follow.
    let segment = server.newest_wal_segment().expect("a WAL segment");
    let mut wal = OpenOptions::new().append(true).open(segment)?;
    wal.write_all(&[0x00, 0x00, 0x10, 0x00, b'a', b'b', b'c'])?;
    drop(wal);

    server.launch()?;

    assert!(server.get_doc("articles", &id)?.is_some());
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 2 regressions (continued)
// ---------------------------------------------------------------------------

/// Concurrent writes followed immediately by a graceful restart must all survive.
///
/// Before Phase 2, about 10-15% of these writes were lost: acknowledged before
/// they reached the WAL, and still queued when the process exited.
#[test]
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

// ---------------------------------------------------------------------------
// Phase 2 features
// ---------------------------------------------------------------------------

#[test]
fn ttl_expires_documents() -> Result<()> {
    let mut server = TestServer::start()?;
    let short = server.insert_with_query("articles", "?ttl=1", &json!({ "title": "Short lived" }))?;
    let long = server.insert("articles", &json!({ "title": "Long lived" }))?;
    assert!(server.get_doc("articles", &short)?.is_some());

    thread::sleep(Duration::from_millis(1500));

    assert!(server.get_doc("articles", &short)?.is_none(), "expired document is still visible");
    assert_eq!(server.count("articles")?, 1);

    server.flush()?;
    server.restart()?;
    assert!(server.get_doc("articles", &short)?.is_none());
    assert!(server.get_doc("articles", &long)?.is_some());
    Ok(())
}

#[test]
fn deleted_tessellation_stays_deleted_after_crash() -> Result<()> {
    let mut server = TestServer::start()?;
    let id = server.insert("scratch", &json!({ "title": "Temporary" }))?;
    server.delete_tessellation("scratch")?;

    // The insert is still in the WAL; replay must not bring it back.
    server.crash_and_restart()?;

    assert!(server.get_doc("scratch", &id)?.is_none());
    assert_eq!(server.count("scratch")?, 0);
    Ok(())
}

#[test]
fn unsafe_tessellation_names_are_rejected() -> Result<()> {
    let server = TestServer::start()?;
    for name in ["has.dot", "bad%20name", "x".repeat(65).as_str()] {
        let status = server.post_status(&format!("/{}", name), &json!({ "title": "x" }))?;
        assert_eq!(status.as_u16(), 400, "name {:?} should be rejected", name);
    }
    Ok(())
}
