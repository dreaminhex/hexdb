//! SQL over POST /sql: translation, paging, aggregates, parameters, and the
//! same permissions, row filters and field masks as the REST API.

use anyhow::Result;
use hexdb_tests::{ApiResponse, TestServer};
use reqwest::Method;
use serde_json::{json, Value};

fn sql(server: &TestServer, body: Value) -> Result<ApiResponse> {
    server.request(Method::POST, "/sql", Some(&body), &[])
}

fn rows(res: &ApiResponse) -> Value {
    assert_eq!(res.status.as_u16(), 200, "{}", res.body);
    res.body["rows"].clone()
}

fn seed(server: &TestServer) -> Result<()> {
    let orders: Vec<Value> = (0..25)
        .map(|i| {
            let customer = ["ada", "bo", "cy"][i % 3];
            json!({
                "n": i,
                "customer": customer,
                "status": if i % 4 == 0 { "refunded" } else { "paid" },
                "total": i * 10,
                "rush": i % 5 == 0,
                "address": { "city": if i % 2 == 0 { "Oslo" } else { "Lima" } },
                "note": if i % 6 == 0 { Value::Null } else { json!(format!("order {}", i)) },
            })
        })
        .collect();
    let res = server.request(Method::POST, "/orders/_bulk", Some(&json!(orders)), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    Ok(())
}

#[test]
fn select_where_order_and_page() -> Result<()> {
    let server = TestServer::start()?;
    seed(&server)?;

    let res = sql(&server, json!({ "sql": "SELECT n, customer, address.city AS city FROM orders WHERE total >= 100 AND customer <> 'bo' ORDER BY n DESC LIMIT 3" }))?;
    assert_eq!(rows(&res), json!([[24, "ada", "Oslo"], [23, "cy", "Lima"], [21, "ada", "Lima"]]));
    assert_eq!(res.body["columns"], json!([
        { "name": "n", "type": "integer" },
        { "name": "customer", "type": "string" },
        { "name": "city", "type": "string" },
    ]));
    assert!(res.body["next"].is_null());
    assert_eq!(res.body["translated"]["tessellation"], "orders");

    // NULL rules: <> and NOT LIKE skip null notes; IS NULL finds them.
    let count = |cond: &str| -> Result<Value> {
        Ok(rows(&sql(&server, json!({ "sql": format!("SELECT COUNT(*) FROM orders WHERE {}", cond) }))?)[0][0].clone())
    };
    assert_eq!(count("note IS NULL")?, 5);
    assert_eq!(count("note <> 'order 1'")?, 19);
    assert_eq!(count("note NOT LIKE 'order 1%'")?, 11);
    assert_eq!(count("NOT (rush OR status = 'refunded')")?, 15);
    assert_eq!(count("customer IN ('ada', 'cy') AND n BETWEEN 3 AND 9")?, 5);
    assert_eq!(rows(&sql(&server, json!({ "sql": "SELECT COUNT(*) FROM orders o WHERE o.address.city = 'Oslo'" }))?), json!([[13]]));

    // SELECT * pages by cursor; every row arrives exactly once.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut body = json!({ "sql": "SELECT * FROM orders WHERE status = ?", "params": ["paid"], "page_size": 7 });
        if let Some(c) = &cursor {
            body["cursor"] = json!(c);
        }
        let res = sql(&server, body)?;
        let names: Vec<&str> = res.body["columns"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names[0], "id");
        let n_at = names.iter().position(|n| *n == "n").unwrap();
        seen.extend(rows(&res).as_array().unwrap().iter().map(|r| r[n_at].as_i64().unwrap()));
        pages += 1;
        match res.body["next"].as_str() {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
    }
    seen.sort();
    assert_eq!(seen, (0..25).filter(|i| i % 4 != 0).collect::<Vec<i64>>());
    assert_eq!(pages, 3);

    // Sorted paging with OFFSET and LIMIT across pages.
    let first = sql(&server, json!({ "sql": "SELECT n FROM orders ORDER BY total LIMIT 5 OFFSET 2", "page_size": 3 }))?;
    assert_eq!(rows(&first), json!([[2], [3], [4]]));
    let second = sql(&server, json!({ "sql": "SELECT n FROM orders ORDER BY total LIMIT 5 OFFSET 2", "page_size": 3, "cursor": first.body["next"] }))?;
    assert_eq!(rows(&second), json!([[5], [6]]));
    assert!(second.body["next"].is_null());

    // SELECT without FROM.
    assert_eq!(rows(&sql(&server, json!({ "sql": "SELECT 1 AS one, 'x'" }))?), json!([[1, "x"]]));
    Ok(())
}

#[test]
fn group_by_having_and_counts() -> Result<()> {
    let server = TestServer::start()?;
    seed(&server)?;

    let res = sql(&server, json!({ "sql": "
        SELECT customer, COUNT(*) AS orders, SUM(total) AS revenue, MAX(n)
        FROM orders
        WHERE status = 'paid'
        GROUP BY customer
        HAVING COUNT(*) > 5
        ORDER BY revenue DESC" }))?;
    let paid: Vec<i64> = (0..25).filter(|i| i % 4 != 0).collect();
    let group = |c: i64| paid.iter().filter(|i| *i % 3 == c).copied().collect::<Vec<_>>();
    let expected: Vec<Value> = {
        let mut out: Vec<(i64, Value)> = Vec::new();
        for (c, name) in ["ada", "bo", "cy"].iter().enumerate() {
            let g = group(c as i64);
            if g.len() > 5 {
                let revenue: i64 = g.iter().map(|i| i * 10).sum();
                out.push((revenue, json!([name, g.len(), revenue, g.iter().max()])));
            }
        }
        out.sort_by_key(|o| std::cmp::Reverse(o.0));
        out.into_iter().map(|(_, v)| v).collect()
    };
    assert_eq!(rows(&res), json!(expected));
    assert_eq!(res.body["columns"][3]["name"], "MAX(n)");

    assert_eq!(rows(&sql(&server, json!({ "sql": "SELECT COUNT(*) AS n FROM orders" }))?), json!([[25]]));
    assert_eq!(rows(&sql(&server, json!({ "sql": "SELECT COUNT(*), SUM(total) FROM orders WHERE n > 100" }))?), json!([[0, null]]));
    assert_eq!(
        rows(&sql(&server, json!({ "sql": "SELECT DISTINCT address.city FROM orders ORDER BY 1" }))?),
        json!([["Lima"], ["Oslo"]])
    );
    assert_eq!(
        rows(&sql(&server, json!({ "sql": "SELECT status, COUNT(DISTINCT customer) FROM orders GROUP BY status ORDER BY status" }))?),
        json!([["paid", 3], ["refunded", 3]])
    );
    Ok(())
}

#[test]
fn errors_and_permissions() -> Result<()> {
    let server = TestServer::start()?;
    seed(&server)?;

    let error = |body: Value, status: u16| -> Result<String> {
        let res = sql(&server, body)?;
        assert_eq!(res.status.as_u16(), status, "{}", res.body);
        Ok(res.body["error"]["message"].as_str().unwrap_or_default().to_string())
    };
    assert!(error(json!({ "sql": "UPDATE orders SET n = 1" }), 400)?.contains("read-only"));
    assert!(error(json!({ "sql": "SELECT * FROM orders o JOIN users u ON o.n = u.n" }), 400)?.contains("JOIN"));
    assert!(error(json!({ "sql": "SELECT * FROM orders WHERE" }), 400)?.contains("sql:"));
    assert!(error(json!({ "sql": "SELECT * FROM orders WHERE n = ?" }), 400)?.contains("no value"));
    error(json!({ "sql": "SELECT * FROM missing" }), 404)?;
    error(json!({ "sql": "SELECT * FROM _users" }), 400)?;
    error(json!({ "sql": "SELECT 1", "page_size": 0 }), 400)?;
    error(json!({ "sql": "SELECT n FROM orders", "cursor": "nonsense" }), 400)?;

    // A role restricted to EU rows without costs sees the same through SQL.
    let role = json!({
        "name": "eu-reader",
        "permissions": ["read"],
        "restrictions": { "eu": { "filter": { "region": "EU" }, "hide": ["cost"] } }
    });
    assert_eq!(server.request(Method::POST, "/roles", Some(&role), &[])?.status.as_u16(), 201);
    for (i, region) in ["EU", "US", "EU"].iter().enumerate() {
        server.insert("eu", &json!({ "n": i, "region": region, "cost": 5 }))?;
    }
    let reader = server.user_with_roles("eu-reader", json!([{ "name": "eu-reader", "tessellations": ["eu"] }]))?;
    let as_reader = |text: &str| server.request_as(Some(&reader), Method::POST, "/sql", Some(&json!({ "sql": text })), &[]);
    assert_eq!(rows(&as_reader("SELECT n, region, cost FROM eu ORDER BY n")?), json!([[0, "EU", null], [2, "EU", null]]));
    assert_eq!(rows(&as_reader("SELECT COUNT(*) FROM eu")?), json!([[2]]));
    assert_eq!(rows(&as_reader("SELECT region, COUNT(*) FROM eu GROUP BY region")?), json!([["EU", 2]]));
    assert_eq!(as_reader("SELECT n FROM eu WHERE cost > 1")?.status.as_u16(), 403);
    assert_eq!(as_reader("SELECT n FROM orders")?.status.as_u16(), 403);

    // The catalog shows the reader only what it can read, without hidden fields.
    let tables = server.request_as(Some(&reader), Method::GET, "/sql/tables", None, &[])?;
    assert_eq!(tables.body["tables"], json!([{ "name": "eu" }]), "{}", tables.body);
    let columns = server.request_as(Some(&reader), Method::GET, "/sql/columns?table=eu", None, &[])?;
    let names: Vec<&str> = columns.body["columns"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["id", "n", "region"], "{}", columns.body);
    assert_eq!(server.request_as(Some(&reader), Method::GET, "/sql/columns?table=orders", None, &[])?.status.as_u16(), 403);
    Ok(())
}

#[test]
fn catalog_columns_come_from_schemas_and_samples() -> Result<()> {
    let server = TestServer::start()?;
    seed(&server)?;
    let res = server.request(Method::GET, "/sql/columns?table=orders", None, &[])?;
    assert_eq!(res.status.as_u16(), 200, "{}", res.body);
    let columns = res.body["columns"].as_array().unwrap();
    let find = |name: &str| columns.iter().find(|c| c["name"] == name).cloned().unwrap_or(Value::Null);
    assert_eq!(columns[0], json!({ "name": "id", "type": "string", "nullable": false, "source": "schema" }));
    assert_eq!(find("total")["type"], "integer");
    assert_eq!(find("address")["type"], "json");
    assert_eq!(find("note")["source"], "sample");

    let schema = json!({ "fields": {
        "sku": { "type": "string", "required": true },
        "price": { "type": "number" },
        "dims.width": { "type": "number" },
    }, "additional_fields": false });
    server.request(Method::POST, "/tessellations", Some(&json!({ "name": "products" })), &[])?;
    let res = server.request(Method::POST, "/tessellations/products/schemas", Some(&schema), &[])?;
    assert_eq!(res.status.as_u16(), 201, "{}", res.body);
    let res = server.request(Method::GET, "/sql/columns?table=products", None, &[])?;
    assert_eq!(res.body["columns"], json!([
        { "name": "id", "type": "string", "nullable": false, "source": "schema" },
        { "name": "dims", "type": "json", "nullable": true, "source": "schema" },
        { "name": "price", "type": "number", "nullable": true, "source": "schema" },
        { "name": "sku", "type": "string", "nullable": false, "source": "schema" },
    ]));
    assert_eq!(server.request(Method::GET, "/sql/columns?table=nothing", None, &[])?.status.as_u16(), 404);

    // Result columns take the schema's types, and an empty SELECT * still lists them.
    server.insert("products", &json!({ "sku": "a-1", "price": 3 }))?;
    let res = sql(&server, json!({ "sql": "SELECT sku, price FROM products" }))?;
    assert_eq!(res.body["columns"], json!([{ "name": "sku", "type": "string" }, { "name": "price", "type": "number" }]));
    let res = sql(&server, json!({ "sql": "SELECT * FROM products WHERE sku = 'none'" }))?;
    assert_eq!(rows(&res), json!([]));
    assert_eq!(res.body["columns"], json!([
        { "name": "id", "type": "string" },
        { "name": "dims", "type": "null" },
        { "name": "price", "type": "number" },
        { "name": "sku", "type": "string" },
    ]));
    Ok(())
}
