//! GraphQL schema tests against a real engine in a temporary directory.

use async_graphql::{Request, Variables};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use hexdb_core::{users, HexConfig, HexDBEngine, HexIdentity};
use hexdb_query::{build_schema, HexDBSchema};
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;
use ulid::Ulid;

async fn setup() -> (TempDir, Arc<HexDBEngine>, HexDBSchema) {
    let dir = TempDir::new().unwrap();
    let mut config = HexConfig::default();
    config.storage.path = dir.path().join("data").to_string_lossy().into_owned();
    config.storage.encryption_key = format!("base64:{}", STANDARD.encode([3u8; 32]));
    let identity = HexIdentity { id: Ulid::new(), name: "Test".into(), hex_type: "Overseer".into() };
    let engine = Arc::new(HexDBEngine::open(config.clone(), identity, &[3u8; 32]).await.unwrap());
    users::bootstrap(&engine, &config.security).await.unwrap();
    let schema = build_schema(engine.clone());
    (dir, engine, schema)
}

async fn run(schema: &HexDBSchema, query: &str, variables: Value) -> Value {
    let response = schema.execute(Request::new(query).variables(Variables::from_json(variables))).await;
    serde_json::to_value(response).unwrap()
}

fn error_code(response: &Value) -> Option<&str> {
    response.pointer("/errors/0/extensions/code").and_then(Value::as_str)
}

#[tokio::test]
async fn insert_query_filter_sort_and_page() {
    let (_dir, engine, schema) = setup().await;

    let docs: Vec<Value> = (0..30)
        .map(|i| json!({ "n": i, "status": if i % 3 == 0 { "draft" } else { "live" }, "tags": if i % 2 == 0 { json!(["even"]) } else { json!(["odd"]) } }))
        .collect();
    let res = run(
        &schema,
        "mutation($docs: [JSON!]!) { insertDocuments(tessellation: \"posts\", documents: $docs) { id } }",
        json!({ "docs": docs }),
    )
    .await;
    assert!(res.get("errors").is_none(), "{}", res);
    assert_eq!(res["data"]["insertDocuments"].as_array().unwrap().len(), 30);

    // Filter + sort + offset paging.
    let query = r#"
        query($filter: JSON) {
          documents(tessellation: "posts", filter: $filter, sort: [{ field: "n", descending: true }], limit: 3, offset: 1) {
            total
            next
            documents { data field(path: "n") }
          }
        }"#;
    let res = run(&schema, query, json!({ "filter": { "status": "live", "tags": "even", "n": { "$lt": 20 } } })).await;
    assert!(res.get("errors").is_none(), "{}", res);
    let page = &res["data"]["documents"];
    // live (n % 3 != 0) and even and < 20: 2,4,8,10,14,16 -> 6 matches; sorted desc: 16,14,10,8,4,2
    assert_eq!(page["total"], 6);
    assert_eq!(page["next"], Value::Null, "sorted results page with offset");
    let ns: Vec<i64> = page["documents"].as_array().unwrap().iter().map(|d| d["field"].as_i64().unwrap()).collect();
    assert_eq!(ns, vec![14, 10, 8]);

    // Cursor paging in ID order.
    let res = run(&schema, "{ documents(tessellation: \"posts\", limit: 25) { next documents { id } } }", json!({})).await;
    let next = res["data"]["documents"]["next"].as_str().unwrap().to_string();
    let res = run(
        &schema,
        "query($after: ID) { documents(tessellation: \"posts\", limit: 25, after: $after) { next documents { id } } }",
        json!({ "after": next }),
    )
    .await;
    assert_eq!(res["data"]["documents"]["documents"].as_array().unwrap().len(), 5);
    assert_eq!(res["data"]["documents"]["next"], Value::Null);

    let res = run(&schema, "{ count(tessellation: \"posts\", filter: { status: \"draft\" }) }", json!({})).await;
    assert_eq!(res["data"]["count"], 10);

    drop(schema);
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn mutations_and_nested_tessellation_fields() {
    let (_dir, engine, schema) = setup().await;

    let res = run(&schema, "mutation { createTessellation(name: \"blog\") { name kind } }", json!({})).await;
    assert_eq!(res["data"]["createTessellation"], json!({ "name": "blog", "kind": "user" }));

    let res = run(
        &schema,
        "mutation { insertDocument(tessellation: \"blog\", data: { title: \"Hi\", draft: true }, ttl: 3600) { id expiresAt data } }",
        json!({}),
    )
    .await;
    let id = res["data"]["insertDocument"]["id"].as_str().unwrap().to_string();
    assert!(res["data"]["insertDocument"]["expiresAt"].is_string());

    let res = run(
        &schema,
        "mutation($id: ID!) { patchDocument(tessellation: \"blog\", id: $id, data: { draft: null, views: 1 }) { data } }",
        json!({ "id": id }),
    )
    .await;
    assert_eq!(res["data"]["patchDocument"]["data"], json!({ "title": "Hi", "views": 1 }));

    let res = run(
        &schema,
        "mutation { updateDocuments(tessellation: \"blog\", filter: { views: { _gte: 1 } }, update: { views: 2 }) { matched modified } }",
        json!({}),
    )
    .await;
    assert_eq!(res["data"]["updateDocuments"], json!({ "matched": 1, "modified": 1 }));

    let res = run(&schema, "{ tessellation(name: \"blog\") { documentCount documents { documents { json } } } }", json!({})).await;
    assert_eq!(res["data"]["tessellation"]["documentCount"], 1);
    assert_eq!(res["data"]["tessellation"]["documents"]["documents"][0]["json"]["views"], 2);

    let res = run(&schema, "mutation($id: ID!) { deleteDocument(tessellation: \"blog\", id: $id) }", json!({ "id": id })).await;
    assert_eq!(res["data"]["deleteDocument"], true);
    let res = run(&schema, "mutation { deleteTessellation(name: \"blog\") }", json!({})).await;
    assert_eq!(res["data"]["deleteTessellation"], true);

    drop(schema);
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn errors_carry_codes_and_system_data_is_protected() {
    let (_dir, engine, schema) = setup().await;

    let res = run(&schema, "{ documents(tessellation: \"users\") { total } }", json!({})).await;
    assert_eq!(error_code(&res), Some("FORBIDDEN"));

    let res = run(&schema, "{ documents(tessellation: \"nope\") { total } }", json!({})).await;
    assert_eq!(error_code(&res), Some("NOT_FOUND"));

    run(&schema, "mutation { insertDocument(tessellation: \"posts\", data: { a: 1 }) { id } }", json!({})).await;
    let res = run(&schema, "query($f: JSON) { count(tessellation: \"posts\", filter: $f) }", json!({ "f": { "$bogus": 1 } })).await;
    assert_eq!(error_code(&res), Some("INVALID_REQUEST"), "{}", res);

    let res = run(&schema, "mutation { insertDocument(tessellation: \"posts\", data: [1]) { id } }", json!({})).await;
    assert_eq!(error_code(&res), Some("INVALID_REQUEST"));

    let res = run(&schema, "mutation { deleteTessellation(name: \"users\") }", json!({})).await;
    assert_eq!(error_code(&res), Some("FORBIDDEN"));

    // Users and roles are available, without secrets.
    let res = run(&schema, "{ users { login roles { name permissions } } roles { name } }", json!({})).await;
    assert_eq!(res["data"]["users"][0]["login"], "hexdbadmin");
    assert_eq!(res["data"]["roles"].as_array().unwrap().len(), 4);

    let res = run(&schema, "{ users { passwordHash } }", json!({})).await;
    assert!(res.get("errors").is_some(), "password hashes are not in the schema");

    drop(schema);
    engine.shutdown().await.unwrap();
}

#[test]
fn schema_exports_sdl() {
    let sdl = HexDBSchema::build(Default::default(), Default::default(), async_graphql::EmptySubscription)
        .finish()
        .sdl();
    for expected in ["type Query", "type Mutation", "scalar JSON", "input SortInput", "type Document"] {
        assert!(sdl.contains(expected), "missing {:?}", expected);
    }
}

#[tokio::test]
async fn mutations_are_idempotent_with_a_key() {
    let (_dir, engine, schema) = setup().await;
    let insert = r#"mutation($data: JSON!) { insertDocument(tessellation: "orders", data: $data, idempotencyKey: "order-1") { id } }"#;

    let (first, replays) = hexdb_query::execute(&schema, Request::new(insert).variables(Variables::from_json(json!({ "data": { "qty": 1 } })))).await;
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert!(replays.is_empty());
    let first = serde_json::to_value(first).unwrap();

    let (second, replays) = hexdb_query::execute(&schema, Request::new(insert).variables(Variables::from_json(json!({ "data": { "qty": 1 } })))).await;
    let second = serde_json::to_value(second).unwrap();
    assert_eq!(replays, vec!["insertDocument".to_string()]);
    assert_eq!(second["extensions"]["idempotentReplays"], json!(["insertDocument"]));
    assert_eq!(second["data"], first["data"], "the replay returns the original document");
    assert_eq!(engine.count_documents("orders").await.unwrap(), 1);

    // Same key, different arguments.
    let res = run(&schema, insert, json!({ "data": { "qty": 2 } })).await;
    assert_eq!(error_code(&res), Some("UNPROCESSABLE"), "{}", res);

    // Several keyed mutations in one request are tracked separately (by alias).
    let id = first["data"]["insertDocument"]["id"].as_str().unwrap().to_string();
    let both = r#"mutation($id: ID!) {
        a: patchDocument(tessellation: "orders", id: $id, data: { qty: 3 }, idempotencyKey: "patch-1") { data }
        b: insertDocument(tessellation: "orders", data: { qty: 9 }, idempotencyKey: "order-2") { id }
    }"#;
    let (_, replays) = hexdb_query::execute(&schema, Request::new(both).variables(Variables::from_json(json!({ "id": id })))).await;
    assert!(replays.is_empty());
    let (again, replays) = hexdb_query::execute(&schema, Request::new(both).variables(Variables::from_json(json!({ "id": id })))).await;
    assert!(again.errors.is_empty(), "{:?}", again.errors);
    assert_eq!(replays, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(engine.count_documents("orders").await.unwrap(), 2);

    drop(schema);
    engine.shutdown().await.unwrap();
}
