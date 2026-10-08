//! Text analyzers on text indexes, and the query advisor.

use anyhow::Result;
use hexdb_tests::{TestOptions, TestServer};
use reqwest::Method;
use serde_json::{json, Value};

fn ids(res: &hexdb_tests::ApiResponse) -> Vec<String> {
    let mut ids: Vec<String> = res.body["documents"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect();
    ids.sort();
    ids
}

fn search(server: &TestServer, tess: &str, text: &str) -> Result<hexdb_tests::ApiResponse> {
    let res = server.request(Method::POST, &format!("/{}/_query", tess), Some(&json!({ "filter": { "$text": text } })), &[])?;
    assert_eq!(res.status, 200, "{}", res.body);
    Ok(res)
}

#[test]
fn text_indexes_use_their_analyzer() -> Result<()> {
    let server = TestServer::start_with(TestOptions {
        extra_toml: "\n[analyzers.codes]\ndescription = \"Product codes\"\ntokenizer = \"whitespace\"\nfilters = [\"lowercase\", \"edge_ngram:3:10\"]\n".into(),
        ..Default::default()
    })?;
    let a = server.insert("articles", &json!({ "title": "Searching the café", "body": "Runners were running" }))?;
    let b = server.insert("articles", &json!({ "title": "A quiet search", "body": "Nothing else" }))?;

    // The catalog lists built-in and configured analyzers.
    let list = server.request(Method::GET, "/analyzers", None, &[])?;
    let names: Vec<&str> = list.body["analyzers"].as_array().unwrap().iter().filter_map(|a| a["name"].as_str()).collect();
    for name in ["standard", "english", "ngram", "autocomplete", "codes"] {
        assert!(names.contains(&name), "{:?}", names);
    }
    let analyzed = server.request(Method::POST, "/analyzers/_analyze", Some(&json!({ "analyzer": "english", "text": "The runners searched" })), &[])?;
    assert_eq!(analyzed.body["index_tokens"], json!(["runner", "search"]), "{}", analyzed.body);
    assert_eq!(server.request(Method::POST, "/analyzers/_analyze", Some(&json!({ "analyzer": "nope", "text": "x" })), &[])?.status, 400);

    // With the standard analyzer, word forms differ; with english they match.
    assert_eq!(search(&server, "articles", "searches")?.body["total"], 0);
    let res = server.request(Method::POST, "/tessellations/articles/indexes", Some(&json!({ "kind": "text", "fields": ["title", "body"], "analyzer": "english" })), &[])?;
    assert_eq!(res.status, 201, "{}", res.body);
    let found = search(&server, "articles", "searches")?;
    assert_eq!(ids(&found), { let mut v = vec![a.clone(), b.clone()]; v.sort(); v });
    assert_eq!(found.body["plan"]["indexes"], json!(["title_body_text"]));
    assert_eq!(ids(&search(&server, "articles", "run")?), vec![a.clone()]);
    assert_eq!(ids(&search(&server, "articles", "cafe")?), vec![a.clone()], "accents folded");
    assert_eq!(search(&server, "articles", "the")?.body["total"], 0, "stop words alone match nothing");

    // Field indexes can't have analyzers; unknown analyzers are refused.
    assert_eq!(server.request(Method::POST, "/tessellations/articles/indexes", Some(&json!({ "fields": ["title"], "analyzer": "english" })), &[])?.status, 400);
    server.insert("products", &json!({ "code": "HX-2026-A" }))?;
    assert_eq!(server.request(Method::POST, "/tessellations/products/indexes", Some(&json!({ "kind": "text", "fields": ["code"], "analyzer": "nope" })), &[])?.status, 400);

    // A custom analyzer from hexdb.toml: prefixes of whole codes.
    let res = server.request(Method::POST, "/tessellations/products/indexes", Some(&json!({ "kind": "text", "fields": ["code"], "analyzer": "codes" })), &[])?;
    assert_eq!(res.status, 201, "{}", res.body);
    assert_eq!(search(&server, "products", "hx-20")?.body["total"], 1);
    assert_eq!(search(&server, "products", "2026")?.body["total"], 0, "not split on dashes");
    Ok(())
}

const STATUSES: [&str; 3] = ["draft", "live", "gone"];

#[test]
fn the_advisor_suggests_indexes_from_observed_queries() -> Result<()> {
    let server = TestServer::start()?;
    let docs: Vec<Value> = (0..300).map(|i| json!({ "status": STATUSES[i % 3], "views": i, "author": format!("a{}", i % 50) })).collect();
    server.request(Method::POST, "/posts/_bulk", Some(&Value::Array(docs)), &[])?;
    for i in 0..12 {
        server.request(Method::POST, "/posts/_query", Some(&json!({ "filter": { "author": format!("a{}", i), "views": { "$gt": 10 } } })), &[])?;
    }
    let advice = server.request(Method::GET, "/tessellations/posts/advice", None, &[])?;
    assert_eq!(advice.status, 200, "{}", advice.body);
    assert_eq!(advice.body["queries_observed"], 12);
    let first = &advice.body["suggestions"][0];
    assert_eq!(first["action"], "create_index", "{}", advice.body);
    assert_eq!(first["index"]["fields"], json!(["author", "views"]));
    assert_eq!(first["impact"], "high");

    // Creating it makes the queries use it, and the suggestion goes away.
    server.request(Method::POST, "/tessellations/posts/indexes", Some(&first["index"].clone()), &[])?;
    let q = server.request(Method::POST, "/posts/_query", Some(&json!({ "filter": { "author": "a1", "views": { "$gt": 10 } } })), &[])?;
    assert_eq!(q.body["plan"]["indexes"], json!(["author_views"]));
    let advice = server.request(Method::GET, "/tessellations/posts/advice", None, &[])?;
    assert!(advice.body["suggestions"].as_array().unwrap().iter().all(|s| s["index"]["fields"] != json!(["author", "views"])), "{}", advice.body);

    // AI advice reports when it isn't configured rather than failing.
    let ai = server.request(Method::GET, "/tessellations/posts/advice?ai=true", None, &[])?;
    assert_eq!(ai.status, 200);
    assert!(ai.body["ai"]["available"].is_boolean(), "{}", ai.body);
    Ok(())
}
