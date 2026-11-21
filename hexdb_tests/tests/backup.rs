//! Online backups: a consistent copy taken while the server runs, which a
//! fresh server can start from.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::{json, Value};
use std::{fs, path::Path};

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn ids(server: &TestServer, tess: &str) -> Result<Vec<String>> {
    let res = server.request(Method::POST, &format!("/{}/_query", tess), Some(&json!({ "limit": 1000 })), &[])?;
    assert_eq!(res.status.as_u16(), 200, "{}", res.body);
    let mut ids: Vec<String> = res.body["documents"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect();
    ids.sort();
    Ok(ids)
}

#[test]
fn a_backup_taken_while_running_restores_to_its_point_in_time() -> Result<()> {
    let source = TestServer::start()?;
    let docs: Vec<Value> = (0..50).map(|i| json!({ "n": i, "status": if i % 2 == 0 { "new" } else { "paid" } })).collect();
    let res = source.request(Method::POST, "/orders/_bulk", Some(&json!(docs)), &[])?;
    let first: Vec<String> = res.body["ids"].as_array().unwrap().iter().map(|d| d.as_str().unwrap().to_string()).collect();
    for id in &first[..5] {
        source.request(Method::DELETE, &format!("/orders/{}", id), None, &[])?;
    }
    for i in 0..10 {
        source.insert("notes", &json!({ "i": i }))?;
    }
    assert_eq!(source.request(Method::POST, "/tessellations/orders/indexes", Some(&json!({ "fields": ["status"] })), &[])?.status.as_u16(), 201);
    // Some data in SSTables, some only in the WAL.
    source.request(Method::POST, "/flush", None, &[])?;
    for i in 0..20 {
        source.insert("orders", &json!({ "n": 100 + i, "status": "late" }))?;
    }
    // A file with two versions.
    assert_eq!(source.request(Method::POST, "/tessellations", Some(&json!({ "name": "media", "kind": "files" })), &[])?.status.as_u16(), 201);
    let (v1, v2) = (vec![7u8; 300_000], vec![9u8; 200_000]);
    let first_upload = source.send_bytes(Method::PUT, "/media/_files/docs/plan.bin", v1.clone(), &[])?;
    assert!(first_upload.status.is_success(), "{}", first_upload.body);
    let file_id = first_upload.body["id"].as_str().unwrap().to_string();
    assert_eq!(source.send_bytes(Method::PUT, "/media/_files/docs/plan.bin", v2.clone(), &[])?.body["version"], 2);
    let orders_at_backup = ids(&source, "orders")?;
    let notes_at_backup = ids(&source, "notes")?;

    let res = source.request(Method::POST, "/backup", Some(&json!({ "name": "before" })), &[])?;
    assert_eq!(res.status.as_u16(), 201, "{}", res.body);
    assert!(res.body["sequence"].as_u64().unwrap() > 0);
    assert_eq!((res.body["complete"].as_bool(), res.body["slices"].as_u64()), (Some(true), Some(64)), "a lone hex has everything: {}", res.body);
    assert_eq!((res.body["scope"].as_str(), res.body["missing_file_chunks"].as_u64()), (Some("lattice"), Some(0)), "{}", res.body);
    let path = res.body["path"].as_str().unwrap().to_string();
    assert!(Path::new(&path).join("backup.json").is_file());

    // Writes after the backup aren't in it.
    for i in 0..7 {
        source.insert("orders", &json!({ "n": 200 + i, "status": "after" }))?;
    }
    source.request(Method::DELETE, "/tessellations/notes", None, &[])?;

    // The list, a duplicate name and a bad name.
    let list = source.request(Method::GET, "/backups", None, &[])?;
    assert_eq!(list.body["backups"][0]["name"], "before", "{}", list.body);
    assert_eq!(source.request(Method::POST, "/backup", Some(&json!({ "name": "before" })), &[])?.status.as_u16(), 409);
    assert_eq!(source.request(Method::POST, "/backup", Some(&json!({ "name": "../escape" })), &[])?.status.as_u16(), 400);
    // Without a body: a generated name.
    let res = source.request(Method::POST, "/backup", None, &[])?;
    assert_eq!(res.status.as_u16(), 201, "{}", res.body);
    assert!(res.body["name"].as_str().unwrap().starts_with("hexdb-"));
    // Every backup is audited, the refused ones with their error.
    let audit = source.request(Method::GET, "/audit?action=maintenance.backup", None, &[])?.body;
    let events = audit["events"].as_array().unwrap();
    assert_eq!(events.len(), 4, "{}", audit);
    assert_eq!(events.iter().filter(|e| e["outcome"] == "failed" && e["details"]["error"].is_string()).count(), 2, "{}", audit);

    // A fresh server started on the backup has exactly the data at that point.
    let mut restored = TestServer::start()?;
    restored.stop()?;
    fs::remove_dir_all(restored.data_dir())?;
    copy_dir(Path::new(&path), &restored.data_dir())?;
    restored.launch()?;
    assert_eq!(ids(&restored, "orders")?, orders_at_backup);
    assert_eq!(ids(&restored, "notes")?, notes_at_backup);
    let q = restored.request(Method::POST, "/orders/_query", Some(&json!({ "filter": { "status": "late" } })), &[])?;
    assert_eq!(q.body["total"], 20);
    assert_eq!(q.body["plan"]["indexes"], json!(["status"]), "the index definition came along");
    // Both versions of the file, byte for byte.
    let (status, _, current) = restored.get_bytes(&format!("/media/{}/content", file_id), &[])?;
    assert_eq!((status.as_u16(), current == v2), (200, true));
    let (status, _, older) = restored.get_bytes(&format!("/media/{}/content?version=1", file_id), &[])?;
    assert_eq!((status.as_u16(), older == v1), (200, true));
    // And it takes writes of its own.
    restored.insert("orders", &json!({ "n": 999 }))?;
    assert_eq!(ids(&restored, "orders")?.len(), orders_at_backup.len() + 1);
    Ok(())
}
