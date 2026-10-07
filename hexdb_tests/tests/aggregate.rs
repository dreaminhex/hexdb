//! Aggregations over REST and GraphQL.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::{json, Value};

fn seed(server: &TestServer) -> Result<()> {
    let orders = json!([
        { "customer": "ada", "status": "paid", "total": 30, "items": 3 },
        { "customer": "ada", "status": "paid", "total": 12.5, "items": 1 },
        { "customer": "bo", "status": "paid", "total": 8, "items": 2 },
        { "customer": "bo", "status": "refunded", "total": 20, "items": 1 },
        { "customer": "cy", "status": "pending", "total": 99, "items": 9 },
    ]);
    let res = server.request(Method::POST, "/orders/_bulk", Some(&orders), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    Ok(())
}

#[test]
fn aggregate_over_rest() -> Result<()> {
    let server = TestServer::start()?;
    seed(&server)?;

    let body = json!({
        "filter": { "status": { "$ne": "refunded" } },
        "group_by": ["customer"],
        "aggregates": {
            "orders": { "$count": "*" },
            "revenue": { "$sum": "total" },
            "largest": { "$max": "total" },
        },
        "sort": "-revenue",
        "limit": 2,
    });
    let res = server.request(Method::POST, "/orders/_aggregate", Some(&body), &[])?;
    assert_eq!(res.status.as_u16(), 200, "{}", res.body);
    assert_eq!(res.body["total_groups"], 3);
    assert_eq!(res.body["matched"], 4);
    assert_eq!(
        res.body["rows"],
        json!([
            { "customer": "cy", "orders": 1, "revenue": 99, "largest": 99 },
            { "customer": "ada", "orders": 2, "revenue": 42.5, "largest": 30 },
        ])
    );

    // Default: one row with a count.
    let res = server.request(Method::POST, "/orders/_aggregate", Some(&json!({})), &[])?;
    assert_eq!(res.body["rows"], json!([{ "count": 5 }]));

    // Errors are 400s with a useful message; unknown tessellations are 404s.
    let res = server.request(Method::POST, "/orders/_aggregate", Some(&json!({ "aggregates": { "x": { "$median": "total" } } })), &[])?;
    assert_eq!(res.status.as_u16(), 400);
    assert!(res.body["error"]["message"].as_str().unwrap().contains("$median"), "{}", res.body);
    let res = server.request(Method::POST, "/nothing_here/_aggregate", Some(&json!({})), &[])?;
    assert_eq!(res.status.as_u16(), 404);
    Ok(())
}

#[test]
fn aggregate_over_graphql() -> Result<()> {
    let server = TestServer::start()?;
    seed(&server)?;

    let query = r#"{
        aggregate(
            tessellation: "orders",
            groupBy: ["status"],
            aggregates: { n: { _count: "*" }, avg: { _avg: "total" }, items: { _sum: "items" } },
            sort: [{ field: "n", descending: true }, { field: "status" }]
        ) { rows totalGroups matched }
    }"#;
    let res = server.request(Method::POST, "/graphql", Some(&json!({ "query": query })), &[])?;
    assert_eq!(res.status.as_u16(), 200);
    let data: &Value = &res.body["data"]["aggregate"];
    assert!(res.body["errors"].is_null(), "{}", res.body);
    assert_eq!(data["totalGroups"], 3);
    assert_eq!(data["matched"], 5);
    assert_eq!(
        data["rows"],
        json!([
            { "status": "paid", "n": 3, "avg": 50.5 / 3.0, "items": 6 },
            { "status": "pending", "n": 1, "avg": 99.0, "items": 9 },
            { "status": "refunded", "n": 1, "avg": 20.0, "items": 1 },
        ])
    );

    let bad = server.request(
        Method::POST,
        "/graphql",
        Some(&json!({ "query": r#"{ aggregate(tessellation: "orders", aggregates: { x: { _sum: "*" } }) { matched } }"# })),
        &[],
    )?;
    assert_eq!(bad.body["errors"][0]["extensions"]["code"], "INVALID_REQUEST", "{}", bad.body);
    Ok(())
}
