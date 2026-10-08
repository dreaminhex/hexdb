//! Role restrictions: row filters (with the caller's attributes) and field
//! masks, enforced on every read and write path.

use anyhow::Result;
use hexdb_tests::{ApiResponse, TestServer};
use reqwest::Method;
use serde_json::{json, Value};

fn ids(res: &ApiResponse) -> Vec<String> {
    let mut ids: Vec<String> = res.body["documents"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect();
    ids.sort();
    ids
}

#[test]
fn row_filters_and_field_masks_apply_to_every_path() -> Result<()> {
    let server = TestServer::start()?;
    let role = json!({
        "name": "regional",
        "description": "Orders of the user's region, without costs",
        "permissions": ["read", "write"],
        "restrictions": { "orders": { "filter": { "region": { "$user": "attributes.region" } }, "hide": ["cost"] } }
    });
    let res = server.request(Method::POST, "/roles", Some(&role), &[])?;
    assert_eq!(res.status.as_u16(), 201, "{}", res.body);
    // A role whose filter doesn't parse is refused.
    let bad = json!({ "name": "broken", "permissions": ["read"], "restrictions": { "orders": { "filter": { "n": { "$nope": 1 } } } } });
    assert_eq!(server.request(Method::POST, "/roles", Some(&bad), &[])?.status.as_u16(), 400);

    let eu = server.user_with_roles("eu-agent", json!([{ "name": "regional", "tessellations": ["orders", "notes"] }]))?;
    let res = server.request(Method::PATCH, "/users/eu-agent", Some(&json!({ "attributes": { "region": "EU" } })), &[])?;
    assert_eq!(res.status.as_u16(), 200, "{}", res.body);
    assert_eq!(res.body["attributes"]["region"], "EU");
    let nobody = server.user_with_roles("no-region", json!([{ "name": "regional", "tessellations": ["orders"] }]))?;

    let mut eu_ids = Vec::new();
    let mut us_ids = Vec::new();
    for i in 0..6 {
        let region = if i % 2 == 0 { "EU" } else { "US" };
        let id = server.insert("orders", &json!({ "n": i, "region": region, "cost": 10 * i, "total": 100 + i }))?;
        if region == "EU" { eu_ids.push(id) } else { us_ids.push(id) }
    }
    eu_ids.sort();
    let eu_doc = eu_ids[0].clone();
    let us_doc = us_ids[0].clone();
    let as_eu = |method: Method, path: &str, body: Option<&Value>| server.request_as(Some(&eu), method, path, body, &[]);

    // Reads: only EU orders, without cost.
    let list = as_eu(Method::GET, "/orders?limit=100", None)?;
    assert_eq!(ids(&list), eu_ids, "{}", list.body);
    assert_eq!(list.body["total"], 3);
    assert!(list.body["documents"].as_array().unwrap().iter().all(|d| d.get("cost").is_none() && d["total"].is_number()));
    assert_eq!(as_eu(Method::GET, &format!("/orders/{}", us_doc), None)?.status.as_u16(), 404);
    let one = as_eu(Method::GET, &format!("/orders/{}", eu_doc), None)?;
    assert_eq!(one.status.as_u16(), 200);
    assert!(one.body.get("cost").is_none() && one.body["region"] == "EU", "{}", one.body);
    assert_eq!(as_eu(Method::GET, "/orders/count", None)?.body["count"], 3);
    assert_eq!(as_eu(Method::GET, "/tessellations/orders", None)?.body["document_count"], 3);
    let agg = as_eu(Method::POST, "/orders/_aggregate", Some(&json!({ "group_by": ["region"], "aggregates": { "n": { "$count": "*" }, "sum": { "$sum": "total" } } })))?;
    assert_eq!(agg.body["rows"], json!([{ "region": "EU", "n": 3, "sum": 306 }]), "{}", agg.body);
    // Hidden fields can't be probed.
    for body in [
        json!({ "filter": { "cost": { "$gt": 10 } } }),
        json!({ "sort": "-cost" }),
        json!({ "filter": { "$text": "anything" } }),
    ] {
        let res = as_eu(Method::POST, "/orders/_query", Some(&body))?;
        assert_eq!(res.status.as_u16(), 403, "{} -> {}", body, res.body);
    }
    assert_eq!(as_eu(Method::POST, "/orders/_aggregate", Some(&json!({ "aggregates": { "c": { "$sum": "cost" } } })))?.status.as_u16(), 403);
    // A role restricted on one tessellation isn't restricted on another.
    server.insert("notes", &json!({ "text": "hello", "cost": 1 }))?;
    assert_eq!(as_eu(Method::GET, "/notes", None)?.body["documents"][0]["cost"], 1);
    // A user without the attribute sees nothing (never everything).
    assert_eq!(server.request_as(Some(&nobody), Method::GET, "/orders", None, &[])?.body["total"], 0);

    // GraphQL goes through the same checks.
    let gql = as_eu(Method::POST, "/graphql", Some(&json!({ "query": "{ documents(tessellation: \"orders\") { total documents { data } } }" })))?;
    let docs = gql.body["data"]["documents"]["documents"].as_array().cloned().unwrap_or_default();
    assert_eq!(gql.body["data"]["documents"]["total"], 3, "{}", gql.body);
    assert!(docs.iter().all(|d| d["data"]["region"] == "EU" && d["data"].get("cost").is_none()), "{}", gql.body);

    // Writes: only EU documents, never the cost field.
    assert_eq!(as_eu(Method::POST, "/orders", Some(&json!({ "region": "US", "total": 1 })))?.status.as_u16(), 403);
    assert_eq!(as_eu(Method::POST, "/orders", Some(&json!({ "region": "EU", "total": 1, "cost": 5 })))?.status.as_u16(), 403);
    let created = as_eu(Method::POST, "/orders", Some(&json!({ "region": "EU", "total": 1 })))?;
    assert_eq!(created.status.as_u16(), 201, "{}", created.body);
    assert_eq!(as_eu(Method::PATCH, &format!("/orders/{}", eu_doc), Some(&json!({ "cost": 1 })))?.status.as_u16(), 403);
    assert_eq!(as_eu(Method::PATCH, &format!("/orders/{}", eu_doc), Some(&json!({ "region": "US" })))?.status.as_u16(), 403, "can't move a document out of reach");
    assert_eq!(as_eu(Method::PATCH, &format!("/orders/{}", us_doc), Some(&json!({ "total": 1 })))?.status.as_u16(), 404);
    assert_eq!(as_eu(Method::DELETE, &format!("/orders/{}", us_doc), None)?.status.as_u16(), 404);
    // A full replace keeps the hidden cost.
    let res = as_eu(Method::PUT, &format!("/orders/{}", eu_doc), Some(&json!({ "region": "EU", "total": 999 })))?;
    assert_eq!(res.status.as_u16(), 200, "{}", res.body);
    assert!(res.body.get("cost").is_none());
    let stored = server.get_doc("orders", &eu_doc)?.unwrap();
    assert_eq!((stored["total"].clone(), stored["cost"].clone()), (json!(999), json!(0)), "{}", stored);
    // Update by filter touches only what the caller can see.
    let res = as_eu(Method::POST, "/orders/_update", Some(&json!({ "filter": {}, "update": { "status": "seen" } })))?;
    assert_eq!(res.body["matched"], 4, "{}", res.body);
    assert!(server.get_doc("orders", &us_doc)?.unwrap().get("status").is_none());
    // Upsert can't take over a document outside the filter.
    let res = as_eu(Method::POST, "/orders/_upsert", Some(&json!({ "key": ["n"], "documents": [{ "n": 1, "region": "EU" }] })))?;
    assert_eq!(res.status.as_u16(), 409, "{}", res.body);
    // Transactions: documents outside the filter don't exist; results are masked.
    let tx = as_eu(Method::POST, "/transactions", Some(&json!({ "operations": [{ "op": "get", "tessellation": "orders", "id": us_doc }] })))?;
    assert_eq!(tx.status.as_u16(), 404, "{}", tx.body);
    let tx = as_eu(Method::POST, "/transactions", Some(&json!({ "operations": [{ "op": "get", "tessellation": "orders", "id": eu_doc }] })))?;
    assert!(tx.body["results"][0]["document"].get("cost").is_none(), "{}", tx.body);

    // The change feed shows only what the caller may see.
    let feed = as_eu(Method::GET, "/changes?after=0&tessellation=orders&limit=1000", None)?;
    let changes = feed.body["changes"].as_array().unwrap();
    assert!(!changes.is_empty());
    assert!(changes.iter().filter(|c| c["op"] == "put").all(|c| c["document"]["region"] == "EU" && c["document"].get("cost").is_none()), "{}", feed.body);

    // Managing needs unrestricted access; restricted roles can't feed streams.
    assert_eq!(as_eu(Method::POST, "/tessellations/orders/indexes", Some(&json!({ "fields": ["region"] })))?.status.as_u16(), 403);
    server.request(Method::POST, "/roles", Some(&json!({ "name": "stream-admin", "permissions": ["manage"] })), &[])?;
    server.request(Method::PATCH, "/users/eu-agent", Some(&json!({ "roles": [{ "name": "regional", "tessellations": ["orders"] }, { "name": "stream-admin", "tessellations": ["stream:*"] }] })), &[])?;
    let res = as_eu(Method::POST, "/streams", Some(&json!({ "name": "leak", "retention_hours": 1, "sources": [{ "tessellation": "orders" }], "destinations": [] })))?;
    assert_eq!(res.status.as_u16(), 403, "{}", res.body);

    // Administrators are never restricted.
    assert_eq!(server.request(Method::GET, "/orders?limit=100", None, &[])?.body["total"], 7);
    Ok(())
}
