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
