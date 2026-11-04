//! Storage end-to-end tests: documents written through the API must read back
//! correctly, including across graceful restarts, crashes, and flushes to SSTables.
//! (Single-document create, patch, replace and delete over REST are in api.rs.)

use anyhow::Result;
use hexdb_tests::{field, TestServer};
use serde_json::json;
use std::{fs::OpenOptions, io::Write, thread, time::Duration};

#[test]
fn values_round_trip_with_their_types() -> Result<()> {
    let server = TestServer::start()?;
    let id = server.insert(
        "articles",
        &json!({
            "title": "Quantum Tessellation", "views": 445, "published": true,
            // Strings stay strings, even ones that look like base64 ("abcd") or a word ("Test").
            "name": "Test", "code": "abcd",
            "tags": ["hexdb", "rust", "ai"], "author": { "name": "Ada", "id": 7 },
        }),
    )?;

    assert_eq!(server.count("articles")?, 1);
    let doc = server.get_doc("articles", &id)?.expect("document should exist");
    assert_eq!(field(&doc, "title"), Some(json!("Quantum Tessellation")));
    assert_eq!(field(&doc, "views"), Some(json!(445)));
    assert_eq!(field(&doc, "published"), Some(json!(true)));
    assert_eq!(field(&doc, "name"), Some(json!("Test")));
    assert_eq!(field(&doc, "code"), Some(json!("abcd")));
    assert_eq!(field(&doc, "tags"), Some(json!(["hexdb", "rust", "ai"])));
    assert_eq!(field(&doc, "author"), Some(json!({ "name": "Ada", "id": 7 })));
    Ok(())
}

/// Writes in SSTables, writes only in the WAL, and changes to flushed documents
/// all survive a graceful restart, with deletes and updates winning over older versions.
/// Each case has its own tessellation, so the counts check exactly that case.
#[test]
fn writes_survive_restart_whether_flushed_or_not() -> Result<()> {
    let mut server = TestServer::start()?;

    // Flushed to SSTables, then a delete and an update that exist only in the WAL
    // (before Phase 2, the older flushed versions came back after a restart).
    let flushed: Vec<String> = (0..5).map(|i| server.insert("flushed", &json!({ "title": format!("Doc {}", i) }))).collect::<Result<_>>()?;
    let deleted_after_flush = server.insert("deleted_after_flush", &json!({ "title": "Doomed" }))?;
    let updated_after_flush = server.insert("updated_after_flush", &json!({ "title": "Draft", "views": 1 }))?;
    server.flush()?;
    server.delete("deleted_after_flush", &deleted_after_flush)?;
    server.patch("updated_after_flush", &json!({ "id": updated_after_flush, "views": 2 }))?;

    // Never flushed: inserts, and a delete of a document that was never on disk.
    let unflushed = server.insert("unflushed", &json!({ "title": "Persistent" }))?;
    server.insert("unflushed", &json!({ "title": "Also persistent" }))?;
    let deleted_unflushed = server.insert("deleted_unflushed", &json!({ "title": "Doomed" }))?;
    server.delete("deleted_unflushed", &deleted_unflushed)?;

    server.restart()?;

    assert_eq!(server.count("flushed")?, 5);
    for id in &flushed {
        assert!(server.get_doc("flushed", id)?.is_some(), "flushed document {} is missing", id);
    }
    assert!(server.get_doc("deleted_after_flush", &deleted_after_flush)?.is_none(), "deleted document came back");
    assert_eq!(server.count("deleted_after_flush")?, 0);
    let doc = server.get_doc("updated_after_flush", &updated_after_flush)?.expect("updated document should exist");
    assert_eq!(field(&doc, "views"), Some(json!(2)), "an older version won after restart");
    assert_eq!(server.count("unflushed")?, 2);
    let doc = server.get_doc("unflushed", &unflushed)?.expect("document should survive restart");
    assert_eq!(field(&doc, "title"), Some(json!("Persistent")));
    assert!(server.get_doc("deleted_unflushed", &deleted_unflushed)?.is_none());
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
    assert_eq!(server.get("/scratch/count")?.status().as_u16(), 404, "the tessellation should be gone");
    Ok(())
}
