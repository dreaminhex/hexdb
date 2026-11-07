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
    assert_eq!(items["links"][0], json!({ "field": "order_id", "tessellation": "orders", "on_delete": "cascade" }));

    // The parent lists its children.
    let orders = server.request(Method::GET, "/tessellations/orders", None, &[])?.body;
    assert_eq!(orders["children"], json!([{ "tessellation": "order_items", "field": "order_id", "on_delete": "cascade" }]), "{}", orders);

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
