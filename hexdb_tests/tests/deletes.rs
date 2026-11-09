//! Parent links, cascading deletes, delete guardrails, the document trash and
//! restoring delete batches.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::{json, Value};

fn add_schema(server: &TestServer, tess: &str, schema: &Value) -> Result<hexdb_tests::ApiResponse> {
    server.request(Method::POST, &format!("/tessellations/{}/schemas", tess), Some(schema), &[])
}

#[test]
fn parent_links_are_checked_indexed_and_shown() -> Result<()> {
    let server = TestServer::start()?;
    server.insert("orders", &json!({ "customer": "ada" }))?;
    server.insert("order_items", &json!({ "sku": "A" }))?;

    // A link to a tessellation that doesn't exist is refused.
    let missing = json!({ "fields": { "order_id": { "type": "string", "parent": { "tessellation": "nope", "on_delete": "cascade" } } } });
    let res = add_schema(&server, "order_items", &missing)?;
    assert_eq!(res.status, 400, "{}", res.body);
    assert!(res.body["error"]["message"].as_str().unwrap().contains("doesn't exist"), "{}", res.body);

    let linked = json!({ "fields": {
        "order_id": { "type": "string", "parent": { "tessellation": "orders", "on_delete": "cascade" } },
        "parent_id": { "type": "string", "nullable": true, "parent": { "tessellation": "order_items", "on_delete": "detach" } },
    } });
    let res = add_schema(&server, "order_items", &linked)?;
    assert_eq!(res.status, 201, "{}", res.body);

    // Each link field is indexed.
    let items = server.request(Method::GET, "/tessellations/order_items", None, &[])?.body;
    let indexes: Vec<&str> = items["indexes"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(indexes.contains(&"order_id") && indexes.contains(&"parent_id"), "{}", items);
    assert_eq!(items["links"][0], json!({ "field": "order_id", "tessellation": "orders", "on_delete": "cascade", "enforce": false }));

    // The parent lists its children.
    let orders = server.request(Method::GET, "/tessellations/orders", None, &[])?.body;
    assert_eq!(orders["children"], json!([{ "tessellation": "order_items", "field": "order_id", "on_delete": "cascade" }]), "{}", orders);

    // An enforced link refuses a parent that doesn't exist (422 parent_missing).
    let enforced = json!({ "fields": { "order_id": { "type": "string", "parent": { "tessellation": "orders", "on_delete": "cascade", "enforce": true } } } });
    server.insert("receipts", &json!({ "n": 0 }))?;
    assert_eq!(add_schema(&server, "receipts", &enforced)?.status, 201);
    let res = server.request(Method::POST, "/receipts", Some(&json!({ "order_id": "01J00000000000000000000000" })), &[])?;
    assert_eq!((res.status.as_u16(), res.error_code()), (422, Some("parent_missing")), "{}", res.body);
    let order = server.insert("orders", &json!({ "customer": "bo" }))?;
    assert_eq!(server.request(Method::POST, "/receipts", Some(&json!({ "order_id": order })), &[])?.status, 201);
    let links = server.request(Method::GET, "/tessellations/receipts", None, &[])?.body;
    assert_eq!(links["links"][0]["enforce"], true, "{}", links);
    assert_eq!(server.request(Method::DELETE, "/tessellations/receipts", None, &[])?.status, 204);

    // A parent can't be dropped while others link to it; a self-link doesn't block.
    let res = server.request(Method::DELETE, "/tessellations/orders", None, &[])?;
    assert_eq!(res.status, 409, "{}", res.body);
    assert!(res.body["error"]["message"].as_str().unwrap().contains("order_items.order_id"), "{}", res.body);
    assert_eq!(server.request(Method::DELETE, "/tessellations/order_items", None, &[])?.status, 204);
    assert_eq!(server.request(Method::DELETE, "/tessellations/orders", None, &[])?.status, 204);
    Ok(())
}

#[test]
fn deletes_by_filter_need_a_filter_preview_and_guard_their_count() -> Result<()> {
    let server = TestServer::start()?;
    let o1 = server.insert("orders", &json!({ "status": "old" }))?;
    let o2 = server.insert("orders", &json!({ "status": "old" }))?;
    let keep = server.insert("orders", &json!({ "status": "new" }))?;
    server.insert("items", &json!({ "sku": "seed" }))?;
    let res = add_schema(&server, "items", &json!({ "fields": { "order_id": { "type": "string", "parent": { "tessellation": "orders", "on_delete": "cascade" } } } }))?;
    assert_eq!(res.status, 201, "{}", res.body);
    for o in [&o1, &o1, &o2] {
        server.insert("items", &json!({ "order_id": o }))?;
    }
    let count = |tess: &str| -> Result<u64> { Ok(server.request(Method::GET, &format!("/tessellations/{}", tess), None, &[])?.body["document_count"].as_u64().unwrap_or(0)) };

    // No filter, no delete.
    for body in [json!({}), json!({ "filter": {} }), json!({ "filter": null })] {
        let res = server.request(Method::POST, "/orders/_delete", Some(&body), &[])?;
        assert_eq!((res.status.as_u16(), res.error_code()), (400, Some("filter_required")), "{}", res.body);
    }

    // A dry run says what would go, cascade included, and deletes nothing.
    let old = json!({ "filter": { "status": "old" }, "dry_run": true });
    let preview = server.request(Method::POST, "/orders/_delete", Some(&old), &[])?;
    assert_eq!(preview.status, 200, "{}", preview.body);
    assert_eq!((preview.body["matched"].as_u64(), preview.body["total"].as_u64()), (Some(2), Some(5)), "{}", preview.body);
    assert_eq!(preview.body["deleted"], json!({ "orders": 2, "items": 3 }));
    assert!(preview.body.get("batch").is_none() && preview.body["sample"].as_array().is_some_and(|s| !s.is_empty()));
    assert_eq!((count("orders")?, count("items")?), (3, 4));

    // expect guards against deleting more than was previewed.
    server.insert("items", &json!({ "order_id": o2 }))?;
    let res = server.request(Method::POST, "/orders/_delete", Some(&json!({ "filter": { "status": "old" }, "expect": 5 })), &[])?;
    assert_eq!((res.status.as_u16(), res.error_code()), (409, Some("count_changed")), "{}", res.body);
    assert_eq!((count("orders")?, count("items")?), (3, 5));
    let res = server.request(Method::POST, "/orders/_delete", Some(&json!({ "filter": { "status": "old" }, "expect": 6 })), &[])?;
    assert_eq!(res.status, 200, "{}", res.body);
    assert!(res.body["batch"].is_string(), "{}", res.body);
    assert_eq!((count("orders")?, count("items")?), (1, 1));
    assert!(server.get_doc("orders", &keep)?.is_some());

    // A single delete says which batch it made.
    let res = server.request(Method::DELETE, &format!("/orders/{}", keep), None, &[])?;
    assert_eq!(res.status, 204);
    assert!(res.headers.get("x-hexdb-delete-batch").is_some_and(|v| v.len() == 26), "{:?}", res.headers);

    // Transactions: delete_where sees earlier operations and needs a filter too.
    let tx = json!({ "operations": [
        { "op": "insert", "tessellation": "orders", "data": { "status": "temp" } },
        { "op": "delete_where", "tessellation": "orders", "filter": { "status": "temp" } },
    ] });
    let res = server.request(Method::POST, "/transactions", Some(&tx), &[])?;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!((res.body["results"][1]["op"].as_str(), res.body["results"][1]["document"]["deleted"].as_u64()), (Some("delete_where"), Some(1)), "{}", res.body);
    let res = server.request(Method::POST, "/transactions", Some(&json!({ "operations": [{ "op": "delete_where", "tessellation": "items" }] })), &[])?;
    assert_eq!(res.status, 400, "{}", res.body);
    assert!(res.body["error"]["message"].as_str().unwrap().contains("\"all\": true"), "{}", res.body);

    // GraphQL has the same rules.
    let gql = |q: &str| server.request(Method::POST, "/graphql", Some(&json!({ "query": q })), &[]);
    let res = gql(r#"mutation { deleteDocuments(tessellation: "items") { total } }"#)?;
    assert_eq!(res.body["errors"][0]["extensions"]["code"], "FILTER_REQUIRED", "{}", res.body);
    let res = gql(r#"mutation { deleteDocuments(tessellation: "items", all: true, dryRun: true) { total dryRun deleted } }"#)?;
    assert_eq!(res.body["data"]["deleteDocuments"]["total"], 1, "{}", res.body);

    // "all": true deletes everything.
    let res = server.request(Method::POST, "/items/_delete", Some(&json!({ "all": true })), &[])?;
    assert_eq!((res.status.as_u16(), res.body["total"].as_u64()), (200, Some(1)), "{}", res.body);
    assert_eq!(count("items")?, 0);
    Ok(())
}

#[test]
fn trash_history_restore_purge_folders_and_files() -> Result<()> {
    let server = TestServer::start()?;
    let res = server.request(Method::POST, "/tessellations", Some(&json!({ "name": "media", "kind": "files" })), &[])?;
    assert_eq!(res.status, 201, "{}", res.body);
    let upload = |path: &str, bytes: &[u8]| -> Result<Value> {
        let res = server.send_bytes(Method::PUT, &format!("/media/_files/{}", path), bytes.to_vec(), &[])?;
        assert!(res.status.is_success(), "upload {}: {}", path, res.body);
        Ok(res.body)
    };
    let media_count = || -> Result<u64> { Ok(server.request(Method::GET, "/media?limit=100", None, &[])?.body["total"].as_u64().unwrap_or(0)) };
    upload("a/one.txt", b"one")?;
    upload("a/two.txt", b"two")?;
    upload("a/b/three.txt", b"three")?;
    upload("keep/four.txt", b"four")?;

    // A folder: only its own files, unless recursive; the root needs all.
    let res = server.request(Method::DELETE, "/media/_folders/a?recursive=true&dry_run=true", None, &[])?;
    assert_eq!((res.status.as_u16(), res.body["total"].as_u64()), (200, Some(3)), "{}", res.body);
    let res = server.request(Method::DELETE, "/media/_folders?recursive=true", None, &[])?;
    assert_eq!(res.error_code(), Some("filter_required"), "{}", res.body);
    let res = server.request(Method::DELETE, "/media/_folders/a", None, &[])?;
    assert_eq!(res.body["total"], 2, "{}", res.body);
    assert_eq!(media_count()?, 2);
    let res = server.request(Method::DELETE, "/media/_folders/a?recursive=true", None, &[])?;
    assert_eq!(res.body["total"], 1, "{}", res.body);
    let folder_batch = res.body["batch"].as_str().unwrap().to_string();
    assert_eq!(media_count()?, 1);

    // The history lists them; restoring the first folder delete brings both files back, content and all.
    let history = server.request(Method::GET, "/deletes?kind=folder", None, &[])?.body;
    assert_eq!(history["total"], 2, "{}", history);
    let first = history["batches"][1]["batch"].as_str().unwrap().to_string();
    let res = server.request(Method::POST, &format!("/deletes/{}/restore", first), None, &[])?;
    assert_eq!((res.status.as_u16(), res.body["restored"]["media"].as_u64()), (200, Some(2)), "{}", res.body);
    let (status, _, bytes) = server.get_bytes("/media/_files/a/two.txt", &[])?;
    assert_eq!((status.as_u16(), bytes.as_slice()), (200, b"two".as_slice()));
    let res = server.request(Method::POST, &format!("/deletes/{}/restore", first), None, &[])?;
    assert_eq!(res.status, 409, "already restored: {}", res.body);

    // Purging a batch deletes it for good; it can't be restored after.
    let res = server.request(Method::DELETE, &format!("/deletes/{}", folder_batch), None, &[])?;
    assert_eq!((res.status.as_u16(), res.body["purged"].as_u64()), (200, Some(1)), "{}", res.body);
    assert_eq!(server.request(Method::POST, &format!("/deletes/{}/restore", folder_batch), None, &[])?.status, 409);
    assert!(server.request(Method::GET, "/media/_trash", None, &[])?.body["documents"].as_array().unwrap().is_empty());

    // Files can belong to a document: they go to the files trash with it, and come back with it.
    let order = server.insert("orders", &json!({ "n": 1 }))?;
    let res = add_schema(&server, "media", &json!({ "fields": { "order_id": { "type": "string", "parent": { "tessellation": "orders", "on_delete": "cascade" } } } }))?;
    assert_eq!(res.status, 201, "{}", res.body);
    let file = upload("orders/receipt.txt", b"receipt")?;
    let file_id = file["id"].as_str().unwrap();
    assert_eq!(server.request(Method::PATCH, &format!("/media/{}", file_id), Some(&json!({ "order_id": order })), &[])?.status, 200);
    let res = server.request(Method::DELETE, &format!("/orders/{}", order), None, &[])?;
    assert_eq!(res.status, 204, "{}", res.body);
    let batch = res.headers["x-hexdb-delete-batch"].to_str()?.to_string();
    assert_eq!(server.request(Method::GET, &format!("/media/{}", file_id), None, &[])?.status, 404);
    let detail = server.request(Method::GET, &format!("/deletes/{}", batch), None, &[])?.body;
    assert_eq!(detail["counts"], json!({ "orders": 1, "media": 1 }), "{}", detail);
    assert_eq!(server.request(Method::POST, &format!("/deletes/{}/restore", batch), None, &[])?.status, 200);
    assert_eq!(server.get_doc("orders", &order)?.unwrap()["n"], 1);
    assert_eq!(server.request(Method::GET, &format!("/media/{}", file_id), None, &[])?.body["order_id"], order.as_str());

    // The document trash, one document at a time.
    let note = server.insert("notes", &json!({ "text": "keep me" }))?;
    server.request(Method::DELETE, &format!("/notes/{}", note), None, &[])?;
    let trash = server.request(Method::GET, "/notes/_trash", None, &[])?.body;
    assert_eq!((trash["documents"][0]["id"].as_str(), trash["documents"][0]["text"].as_str()), (Some(note.as_str()), Some("keep me")), "{}", trash);
    assert!(trash["documents"][0]["_deleted"]["batch"].is_string());
    assert_eq!(server.request(Method::POST, &format!("/notes/_trash/{}/restore", note), None, &[])?.status, 200);
    assert!(server.get_doc("notes", &note)?.is_some());
    server.request(Method::DELETE, &format!("/notes/{}", note), None, &[])?;
    assert_eq!(server.request(Method::DELETE, &format!("/notes/_trash/{}", note), None, &[])?.status, 204);
    assert_eq!(server.request(Method::POST, &format!("/notes/_trash/{}/restore", note), None, &[])?.status, 404);

    // Writers see their own batches; auditors see all; a writer can't restore into a tessellation they can't write.
    let writer = server.user_with_roles("writer-1", json!([{ "name": "writer", "tessellations": ["notes"] }]))?;
    let mine = server.request_as(Some(&writer), Method::POST, "/notes", Some(&json!({ "text": "w" })), &[])?.body["id"].as_str().unwrap().to_string();
    assert_eq!(server.request_as(Some(&writer), Method::DELETE, &format!("/notes/{}", mine), None, &[])?.status, 204);
    let theirs = server.request_as(Some(&writer), Method::GET, "/deletes", None, &[])?.body;
    assert_eq!(theirs["total"], 1, "{}", theirs);
    assert_eq!(server.request_as(Some(&writer), Method::POST, &format!("/deletes/{}/restore", batch), None, &[])?.status, 403);
    let auditor = server.user_with_roles("auditor-1", json!([{ "name": "auditor", "tessellations": ["*"] }]))?;
    assert!(server.request_as(Some(&auditor), Method::GET, "/deletes", None, &[])?.body["total"].as_u64().unwrap() >= 5);

    // The audit trail has the batches, not plain single deletes.
    let audit = server.request(Method::GET, "/audit?action=data.&limit=100", None, &[])?.body;
    let actions: Vec<&str> = audit["events"].as_array().unwrap().iter().filter_map(|e| e["action"].as_str()).collect();
    for expected in ["data.delete.folder", "data.delete.cascade", "data.restore", "data.purge"] {
        assert!(actions.contains(&expected), "{} missing from {:?}", expected, actions);
    }
    assert_eq!(actions.iter().filter(|a| **a == "data.delete.cascade").count(), 1, "{:?}", actions);
    Ok(())
}

#[test]
fn the_cli_previews_confirms_and_restores() -> Result<()> {
    let server = TestServer::start()?;
    for status in ["old", "old", "new"] {
        server.insert("orders", &json!({ "status": status }))?;
    }
    let cli = |args: &[&str]| -> Result<(bool, String)> {
        let out = std::process::Command::new(hexdb_tests::cli_bin()?)
            .arg("--config")
            .arg(server.dir().join("hexdb.toml"))
            .args(args)
            .env("HEXDB_TOKEN", server.token())
            .current_dir(server.dir())
            .stdin(std::process::Stdio::null())
            .output()?;
        Ok((out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
    };

    // Nothing to delete without a target; a script must say --yes.
    let (ok, text) = cli(&["rm", "orders"])?;
    assert!(!ok && text.contains("--all"), "{}", text);
    let (ok, text) = cli(&["rm", "orders", "--filter", r#"{"status": "old"}"#])?;
    assert!(!ok && text.contains("would delete 2") && text.contains("--yes"), "{}", text);
    assert_eq!(server.count("orders")?, 3);

    let (ok, text) = cli(&["rm", "orders", "--filter", r#"{"status": "old"}"#, "--yes"])?;
    assert!(ok, "{}", text);
    assert_eq!(server.count("orders")?, 1);
    let batch = text.split("hexdb deletes restore ").nth(1).map(|s| s.trim().to_string()).expect("a restore hint");

    let (ok, text) = cli(&["deletes", "list"])?;
    assert!(ok && text.contains(&batch) && text.contains("filter"), "{}", text);
    let (ok, text) = cli(&["trash", "orders"])?;
    assert!(ok && text.contains("2 in the trash"), "{}", text);
    let (ok, text) = cli(&["deletes", "restore", &batch])?;
    assert!(ok && text.contains("2 in orders"), "{}", text);
    assert_eq!(server.count("orders")?, 3);
    let (ok, text) = cli(&["deletes", "purge", &batch])?;
    assert!(!ok && text.contains("--yes"), "{}", text);

    // Documents: an insert sent twice with the same key is written once.
    let (ok, text) = cli(&["doc", "insert", "notes", r#"{"text": "once"}"#, "--idempotency-key", "note-1", "--json"])?;
    assert!(ok, "{}", text);
    let id = serde_json::from_str::<Value>(&text)?["id"].as_str().unwrap().to_string();
    let (ok, text) = cli(&["doc", "insert", "notes", r#"{"text": "once"}"#, "--idempotency-key", "note-1"])?;
    assert!(ok && text.contains("already done") && text.contains(&id), "{}", text);
    assert_eq!(server.count("notes")?, 1);
    let (ok, text) = cli(&["doc", "insert", "notes", r#"[{"text": "a"}, {"text": "b"}]"#])?;
    assert!(ok && text.contains("Inserted 2 documents"), "{}", text);

    // Patch and replace, with a version precondition read from `doc get`.
    let (ok, text) = cli(&["doc", "get", "notes", &id])?;
    assert!(ok, "{}", text);
    let version = serde_json::from_str::<Value>(&text)?["_version"].as_u64().expect("a version");
    let (ok, text) = cli(&["doc", "patch", "notes", &id, r#"{"text": "stale"}"#, "--if-version", &(version + 1).to_string()])?;
    assert!(!ok && text.contains("version"), "{}", text);
    let (ok, text) = cli(&["doc", "patch", "notes", &id, r#"{"text": "patched"}"#, "--if-version", &version.to_string()])?;
    assert!(ok, "{}", text);
    let (ok, text) = cli(&["doc", "replace", "notes", &id, r#"{"body": "replaced"}"#])?;
    assert!(ok, "{}", text);
    assert_eq!(server.get_doc("notes", &id)?.unwrap()["body"], "replaced");

    // Update by filter needs a filter (or --all) and, in a script, --yes.
    let (ok, text) = cli(&["doc", "update", "notes", "--set", r#"{"seen": true}"#])?;
    assert!(!ok && text.contains("--all"), "{}", text);
    let (ok, text) = cli(&["doc", "update", "notes", "--all", "--set", r#"{"seen": true}"#])?;
    assert!(!ok && text.contains("--yes") && text.contains("3 document"), "{}", text);
    let (ok, text) = cli(&["doc", "update", "notes", "--all", "--set", r#"{"seen": true}"#, "--yes"])?;
    assert!(ok && text.contains("Updated 3"), "{}", text);

    // A schema with an enforced parent link, then a transaction and a refused orphan.
    let schema = r#"{"fields": {"order_id": {"type": "string", "parent": {"tessellation": "orders", "on_delete": "cascade", "enforce": true}}}}"#;
    server.insert("lines", &json!({ "order_id": null }))?;
    let (ok, text) = cli(&["schema", "set", "lines", schema])?;
    assert!(ok && text.contains("version 1"), "{}", text);
    let (ok, text) = cli(&["schema", "show", "lines"])?;
    assert!(ok && text.contains("\"enforce\": true"), "{}", text);
    let order = server.insert("orders", &json!({ "status": "tx" }))?;
    let tx = format!(r#"[{{"op": "insert", "tessellation": "lines", "data": {{"order_id": "{}"}}}}]"#, order);
    let (ok, text) = cli(&["tx", &tx])?;
    assert!(ok && text.contains("Committed 1"), "{}", text);
    let (ok, text) = cli(&["doc", "insert", "lines", r#"{"order_id": "01J00000000000000000000000"}"#])?;
    assert!(!ok && text.contains("doesn't exist"), "{}", text);
    Ok(())
}
