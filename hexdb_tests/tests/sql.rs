//! SQL over POST /sql: translation, paging, aggregates, parameters, and the
//! same permissions, row filters and field masks as the REST API.

use anyhow::Result;
use hexdb_tests::{ApiResponse, TestOptions, TestServer};
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
    // System tessellations stay out of reach, joined or not.
    assert!(error(json!({ "sql": "SELECT * FROM orders o JOIN users u ON o.n = u.n" }), 403)?.contains("system tessellation"));
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

/// Customers and their orders, for joins.
fn seed_shop(server: &TestServer) -> Result<()> {
    let customers = json!([
        { "code": "01J00000000000000000000001", "name": "Ada", "country": "NL", "vip": true },
        { "code": "01J00000000000000000000002", "name": "Bo", "country": "BE", "vip": false },
        { "code": "01J00000000000000000000003", "name": "Cy", "country": "NL", "vip": false },
    ]);
    let res = server.request(Method::POST, "/customers/_bulk", Some(&customers), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    let orders = json!([
        { "customer": "01J00000000000000000000001", "total": 120, "shipping": 10, "placed": "2026-01-15T10:00:00Z", "sku": "A_1" },
        { "customer": "01J00000000000000000000001", "total": 80, "shipping": 5, "placed": "2026-04-02T09:30:00Z", "sku": "B-2" },
        { "customer": "01J00000000000000000000002", "total": 40, "shipping": 0, "placed": "2026-04-20T12:00:00Z", "sku": "A-3" },
        { "customer": "nobody", "total": 5, "shipping": 1, "placed": "2026-07-01T00:00:00Z", "sku": "C_4" },
    ]);
    let res = server.request(Method::POST, "/purchases/_bulk", Some(&orders), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    Ok(())
}

fn q(server: &TestServer, text: &str) -> Result<Value> {
    let res = sql(server, json!({ "sql": text }))?;
    assert_eq!(res.status.as_u16(), 200, "{}: {}", text, res.body);
    Ok(res.body["rows"].clone())
}

#[test]
fn joins_subqueries_and_expressions() -> Result<()> {
    let server = TestServer::start()?;
    seed_shop(&server)?;

    // Inner join, with an expression and a filter pushed into each read.
    assert_eq!(
        q(&server, "SELECT c.name, p.total + p.shipping AS paid FROM purchases p JOIN customers c ON p.customer = c.code WHERE c.country = 'NL' ORDER BY paid DESC")?,
        json!([["Ada", 130], ["Ada", 85]])
    );
    // Comma joins with the condition in WHERE are joins too.
    assert_eq!(q(&server, "SELECT COUNT(*) FROM purchases p, customers c WHERE p.customer = c.code")?, json!([[3]]));
    // LEFT, RIGHT and FULL joins keep unmatched rows, with NULLs.
    assert_eq!(
        q(&server, "SELECT c.name, COUNT(p.total) AS orders FROM customers c LEFT JOIN purchases p ON p.customer = c.code GROUP BY c.name ORDER BY c.name")?,
        json!([["Ada", 2], ["Bo", 1], ["Cy", 0]])
    );
    assert_eq!(q(&server, "SELECT p.sku, c.name FROM customers c RIGHT JOIN purchases p ON p.customer = c.code WHERE c.name IS NULL")?, json!([["C_4", null]]));
    assert_eq!(q(&server, "SELECT COUNT(*) FROM customers c FULL JOIN purchases p ON p.customer = c.code")?, json!([[5]]));

    // Aggregates over expressions, HAVING, and grouping by an expression.
    assert_eq!(
        q(&server, "SELECT EXTRACT(QUARTER FROM placed) AS q, SUM(total) AS sales, AVG(shipping) FROM purchases GROUP BY EXTRACT(QUARTER FROM placed) HAVING SUM(total) > 10 ORDER BY q")?,
        json!([[1, 120, 10.0], [2, 120, 2.5]])
    );
    // Comparing two fields, CASE, CAST, string functions, LIKE with _.
    assert_eq!(q(&server, "SELECT sku FROM purchases WHERE shipping * 10 >= total ORDER BY sku")?, json!([["C_4"]]));
    assert_eq!(
        q(&server, "SELECT UPPER(name), CASE WHEN vip THEN 'gold' ELSE 'standard' END, LENGTH(name) || '!' FROM customers ORDER BY name LIMIT 2")?,
        json!([["ADA", "gold", "3!"], ["BO", "standard", "2!"]])
    );
    assert_eq!(q(&server, "SELECT sku FROM purchases WHERE sku LIKE 'A!_%' ESCAPE '!'")?, json!([["A_1"]]));
    assert_eq!(q(&server, "SELECT sku FROM purchases WHERE sku LIKE '_-_' ORDER BY sku")?, json!([["A-3"], ["B-2"]]));
    assert_eq!(q(&server, "SELECT CAST(total AS VARCHAR), CAST('7' AS INTEGER) + 1 FROM purchases WHERE total = 40")?, json!([["40", 8]]));

    // Subqueries: IN, correlated EXISTS, a scalar subquery, and FROM.
    assert_eq!(
        q(&server, "SELECT name FROM customers WHERE code IN (SELECT customer FROM purchases WHERE total > 50) ORDER BY name")?,
        json!([["Ada"]])
    );
    assert_eq!(
        q(&server, "SELECT name FROM customers c WHERE NOT EXISTS (SELECT 1 FROM purchases p WHERE p.customer = c.code)")?,
        json!([["Cy"]])
    );
    assert_eq!(
        q(&server, "SELECT name, (SELECT SUM(total) FROM purchases p WHERE p.customer = c.code) AS spent FROM customers c ORDER BY spent DESC NULLS LAST")?,
        json!([["Ada", 200], ["Bo", 40], ["Cy", null]])
    );
    assert_eq!(
        q(&server, "SELECT big.n FROM (SELECT customer, COUNT(*) AS n FROM purchases GROUP BY customer) AS big WHERE big.n > 1")?,
        json!([[2]])
    );

    // WITH, UNION, INTERSECT and EXCEPT.
    assert_eq!(
        q(&server, "WITH nl AS (SELECT code FROM customers WHERE country = 'NL') SELECT COUNT(*) FROM purchases WHERE customer IN (SELECT code FROM nl)")?,
        json!([[2]])
    );
    assert_eq!(q(&server, "SELECT country FROM customers UNION SELECT 'XX' ORDER BY 1")?, json!([["BE"], ["NL"], ["XX"]]));
    assert_eq!(q(&server, "SELECT country FROM customers UNION ALL SELECT country FROM customers")?.as_array().unwrap().len(), 6);
    assert_eq!(q(&server, "SELECT code FROM customers EXCEPT SELECT customer FROM purchases")?, json!([["01J00000000000000000000003"]]));
    assert_eq!(q(&server, "SELECT code FROM customers INTERSECT SELECT customer FROM purchases ORDER BY code")?.as_array().unwrap().len(), 2);

    // * across a join names each table's columns; results page like any other.
    let res = sql(&server, json!({ "sql": "SELECT * FROM purchases p JOIN customers c ON p.customer = c.code", "page_size": 2 }))?;
    assert_eq!(rows(&res).as_array().unwrap().len(), 2);
    let names: Vec<&str> = res.body["columns"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"total") && names.contains(&"country"), "{:?}", names);
    assert_eq!(res.body["translated"]["executor"], "general");
    let next = res.body["next"].as_str().expect("a second page").to_string();
    let res = sql(&server, json!({ "sql": "SELECT * FROM purchases p JOIN customers c ON p.customer = c.code", "page_size": 2, "cursor": next }))?;
    assert_eq!(rows(&res).as_array().unwrap().len(), 1);
    assert!(res.body["next"].is_null());

    // Mistakes come back as SQL errors.
    let bad = |text: &str| -> Result<String> {
        let res = sql(&server, json!({ "sql": text }))?;
        assert_eq!(res.status.as_u16(), 400, "{}: {}", text, res.body);
        Ok(res.body["error"]["message"].as_str().unwrap_or_default().to_string())
    };
    assert!(bad("SELECT name FROM customers WHERE code = (SELECT customer FROM purchases)")?.contains("more than one row"));
    assert!(bad("SELECT total / 0 FROM purchases")?.contains("division by zero"));
    assert!(bad("SELECT name, COUNT(*) FROM customers GROUP BY country")?.contains("GROUP BY"));
    assert!(bad("SELECT country FROM customers UNION SELECT code, name FROM customers")?.contains("same number of columns"));
    Ok(())
}

#[test]
fn joins_keep_permissions_row_filters_and_hidden_fields() -> Result<()> {
    let server = TestServer::start()?;
    seed_shop(&server)?;
    // A reader of customers in NL without their VIP flag, and of purchases.
    let role = json!({
        "name": "nl-reader",
        "permissions": ["read"],
        "restrictions": { "customers": { "filter": { "country": "NL" }, "hide": ["vip"] } }
    });
    assert_eq!(server.request(Method::POST, "/roles", Some(&role), &[])?.status.as_u16(), 201);
    let reader = server.user_with_roles("nl", json!([{ "name": "nl-reader", "tessellations": ["customers", "purchases"] }]))?;
    let as_reader = |text: &str| server.request_as(Some(&reader), Method::POST, "/sql", Some(&json!({ "sql": text })), &[]);

    // Only NL customers join; Bo's purchase finds no customer.
    assert_eq!(
        rows(&as_reader("SELECT c.name, COUNT(*) FROM purchases p JOIN customers c ON p.customer = c.code GROUP BY c.name")?),
        json!([["Ada", 2]])
    );
    // Hidden fields read as NULL in expressions, and can't be filtered on in a read.
    assert_eq!(
        rows(&as_reader("SELECT name, vip FROM customers c WHERE EXISTS (SELECT 1 FROM purchases p WHERE p.customer = c.code)")?),
        json!([["Ada", null]])
    );
    assert_eq!(as_reader("SELECT name FROM customers WHERE vip = true")?.status.as_u16(), 403);
    // A join, subquery or CTE over a tessellation the reader can't read is refused.
    let other = server.user_with_roles("only-purchases", json!([{ "name": "reader", "tessellations": ["purchases"] }]))?;
    for text in [
        "SELECT * FROM purchases p JOIN customers c ON p.customer = c.code",
        "SELECT * FROM purchases WHERE customer IN (SELECT code FROM customers)",
        "WITH c AS (SELECT * FROM customers) SELECT * FROM c",
    ] {
        let res = server.request_as(Some(&other), Method::POST, "/sql", Some(&json!({ "sql": text })), &[])?;
        assert_eq!(res.status.as_u16(), 403, "{}: {}", text, res.body);
    }
    Ok(())
}

#[test]
fn big_reads_are_refused_not_cut_short() -> Result<()> {
    let server = TestServer::start_with(TestOptions { extra_toml: "\n[limits]\nsql_max_rows = 3\n".into(), ..TestOptions::default() })?;
    seed_shop(&server)?;
    // Four purchases: too many to read whole, but a filter narrows the read.
    let res = sql(&server, json!({ "sql": "SELECT p.sku, c.name FROM purchases p JOIN customers c ON p.customer = c.code" }))?;
    assert_eq!(res.status.as_u16(), 400, "{}", res.body);
    assert!(res.body["error"]["message"].as_str().unwrap().contains("sql_max_rows"), "{}", res.body);
    let narrowed = q(&server, "SELECT p.sku, c.name FROM purchases p JOIN customers c ON p.customer = c.code WHERE p.total >= 80")?;
    assert_eq!(narrowed.as_array().unwrap().len(), 2);
    Ok(())
}
