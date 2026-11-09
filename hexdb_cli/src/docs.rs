// `hexdb doc`, `hexdb tx` and `hexdb schema`: documents, transactions and
// schemas (with their parent links) from the command line.
//
// Every write carries an idempotency key and is retried with it when the
// answer is lost (see api.rs), so running a write twice by accident needs the
// same --idempotency-key to be harmless, and a dropped connection never
// applies a write twice. `hexdb doc update` by filter asks first, like
// `hexdb rm`: it counts the matches, and needs the tessellation's name typed
// for every document or more than 100, y otherwise, or --yes in a script.

use crate::api::{encode, new_key, read_json, Target};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::io::IsTerminal;

/// Above this many documents, confirming an update means typing the tessellation's name.
const TYPE_TO_CONFIRM: u64 = 100;

/// Options every document write takes.
pub struct WriteOptions<'a> {
    pub key: Option<&'a str>,
    pub ttl: Option<u64>,
    pub json: bool,
}

impl WriteOptions<'_> {
    fn key(&self) -> String {
        self.key.map(String::from).unwrap_or_else(new_key)
    }

    fn query(&self) -> String {
        self.ttl.map(|t| format!("?ttl={}", t)).unwrap_or_default()
    }
}

fn print(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn replayed(headers: &reqwest::header::HeaderMap) -> bool {
    headers.get("idempotent-replayed").is_some_and(|v| v == "true")
}

/// `hexdb doc get`, with the document's version (its ETag) as `_version`,
/// for `--if-version`.
pub async fn get(target: &Target<'_>, tess: &str, id: &str) -> Result<()> {
    let (mut doc, headers) = target.read(&format!("/{}/{}", encode(tess), encode(id))).await?;
    let version = headers.get("etag").and_then(|v| v.to_str().ok()).and_then(|v| v.trim_matches('"').parse::<u64>().ok());
    if let (Some(version), Some(map)) = (version, doc.as_object_mut()) {
        map.insert("_version".into(), json!(version));
    }
    print(&doc)
}

/// `hexdb doc insert`: one document, or an array inserted atomically.
pub async fn insert(target: &Target<'_>, tess: &str, data: &str, options: &WriteOptions<'_>) -> Result<()> {
    let data = read_json(data)?;
    let key = options.key();
    let (path, what) = match &data {
        Value::Array(items) => (format!("/{}/_bulk{}", encode(tess), options.query()), format!("{} documents", items.len())),
        Value::Object(_) => (format!("/{}{}", encode(tess), options.query()), "1 document".to_string()),
        _ => bail!("Give a JSON object, or an array of them."),
    };
    let (body, headers) = target.write(reqwest::Method::POST, &path, Some(&data), &key).await?;
    if options.json {
        return print(&body);
    }
    let ids: Vec<String> = match &body {
        Value::Array(docs) => docs.iter().filter_map(|d| d["id"].as_str().map(String::from)).collect(),
        Value::Object(map) => match map.get("ids").or_else(|| map.get("documents")) {
            Some(Value::Array(list)) => list.iter().filter_map(|d| d.as_str().or_else(|| d["id"].as_str()).map(String::from)).collect(),
            _ => body["id"].as_str().map(|s| vec![s.to_string()]).unwrap_or_default(),
        },
        _ => Vec::new(),
    };
    let again = if replayed(&headers) { " (already done: this key's first result)" } else { "" };
    println!("Inserted {} into {}{}: {}", what, tess, again, ids.join(", "));
    Ok(())
}

/// `hexdb doc replace` / `hexdb doc patch`, with an optional version
/// precondition (sent as a one-operation transaction).
pub async fn modify(target: &Target<'_>, op: &str, tess: &str, id: &str, data: &str, if_version: Option<u64>, options: &WriteOptions<'_>) -> Result<()> {
    let data = read_json(data)?;
    if !data.is_object() {
        bail!("Give a JSON object.");
    }
    let key = options.key();
    let (body, headers) = match if_version {
        None => {
            let method = if op == "replace" { reqwest::Method::PUT } else { reqwest::Method::PATCH };
            target.write(method, &format!("/{}/{}{}", encode(tess), encode(id), options.query()), Some(&data), &key).await?
        }
        Some(version) => {
            let mut operation = json!({ "op": op, "tessellation": tess, "id": id, "data": data, "if_version": version });
            if let Some(ttl) = options.ttl {
                operation["ttl"] = json!(ttl);
            }
            let (answer, headers) = target.write(reqwest::Method::POST, "/transactions", Some(&json!({ "operations": [operation] })), &key).await?;
            (answer["results"][0]["document"].clone(), headers)
        }
    };
    if options.json {
        return print(&body);
    }
    let again = if replayed(&headers) { " (already done: this key's first result)" } else { "" };
    println!("{} {} in {}{}.", if op == "replace" { "Replaced" } else { "Patched" }, id, tess, again);
    Ok(())
}

/// `hexdb doc upsert`: insert or replace by key fields.
pub async fn upsert(target: &Target<'_>, tess: &str, on: &[String], data: &str, options: &WriteOptions<'_>) -> Result<()> {
    let data = read_json(data)?;
    let documents = match data {
        Value::Array(items) => items,
        doc @ Value::Object(_) => vec![doc],
        _ => bail!("Give a JSON object, or an array of them."),
    };
    let body = json!({ "key": on, "documents": documents });
    let (answer, headers) = target.write(reqwest::Method::POST, &format!("/{}/_upsert", encode(tess)), Some(&body), &options.key()).await?;
    if options.json {
        return print(&answer);
    }
    let again = if replayed(&headers) { " (already done: this key's first result)" } else { "" };
    println!("Upserted into {}{}: {} inserted, {} replaced.", tess, again, answer["inserted"], answer["replaced"]);
    Ok(())
}

/// `hexdb doc update`: merge-patch every document matching a filter, after
/// saying how many match and asking.
#[allow(clippy::too_many_arguments)]
pub async fn update(target: &Target<'_>, tess: &str, filter: Option<&str>, all: bool, set: &str, yes: bool, options: &WriteOptions<'_>) -> Result<()> {
    let filter = match filter {
        Some(text) => read_json(text)?,
        None => json!({}),
    };
    let everything = filter.as_object().is_none_or(|f| f.is_empty());
    if everything && !all {
        bail!("No filter: this would update every document in '{}'. Give --filter '{{...}}', or --all if that's what you mean.", tess);
    }
    let update = read_json(set)?;
    let matched = target.send(reqwest::Method::POST, &format!("/{}/_query", encode(tess)), Some(&json!({ "filter": filter, "limit": 1 }))).await?["total"]
        .as_u64()
        .unwrap_or(0);
    if matched == 0 {
        println!("Nothing matches; nothing was updated.");
        return Ok(());
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("This would update {} document(s) in '{}'. Not changing them without a terminal to confirm on: add --yes.", matched, tess);
        }
        println!("This would update {} document(s) in '{}' with {}.", matched, tess, update);
        let serious = everything || matched > TYPE_TO_CONFIRM;
        if !crate::deletes::confirm("Update them?", serious.then_some(tess))? {
            println!("Nothing was updated.");
            return Ok(());
        }
    }
    let body = json!({ "filter": filter, "update": update, "all": everything });
    let path = format!("/{}/_update{}", encode(tess), options.query());
    let (answer, headers) = target.write(reqwest::Method::POST, &path, Some(&body), &options.key()).await?;
    if options.json {
        return print(&answer);
    }
    let again = if replayed(&headers) { " (already done: this key's first result)" } else { "" };
    println!("Updated {} of {} matching document(s) in {}{}.", answer["modified"], answer["matched"], tess, again);
    Ok(())
}

/// `hexdb tx`: operations across tessellations, all or nothing.
pub async fn transaction(target: &Target<'_>, data: &str, options: &WriteOptions<'_>) -> Result<()> {
    let data = read_json(data)?;
    let body = if data.is_array() { json!({ "operations": data }) } else { data };
    let (answer, headers) = target.write(reqwest::Method::POST, "/transactions", Some(&body), &options.key()).await?;
    if options.json || replayed(&headers) {
        if replayed(&headers) {
            eprintln!("Already done: this key's first result.");
        }
        return print(&answer);
    }
    println!("Committed {} operation(s), {} write(s).", answer["results"].as_array().map_or(0, Vec::len), answer["writes"]);
    for r in answer["results"].as_array().cloned().unwrap_or_default() {
        let id = r["id"].as_str().filter(|s| !s.is_empty()).map(|s| format!(" {}", s)).unwrap_or_default();
        let deleted = r["document"]["deleted"].as_u64().map(|n| format!(" ({} deleted)", n)).unwrap_or_default();
        println!("  {} {}{}{}", r["op"].as_str().unwrap_or(""), r["tessellation"].as_str().unwrap_or(""), id, deleted);
    }
    Ok(())
}

/// `hexdb schema show`.
pub async fn schema_show(target: &Target<'_>, tess: &str) -> Result<()> {
    let schemas = target.send(reqwest::Method::GET, &format!("/tessellations/{}/schemas", encode(tess)), None).await?;
    let info = target.send(reqwest::Method::GET, &format!("/tessellations/{}", encode(tess)), None).await?;
    print(&json!({ "schemas": schemas, "links": info["links"], "children": info["children"] }))
}

/// `hexdb schema check`: would this version be accepted, and would the documents fit?
pub async fn schema_check(target: &Target<'_>, tess: &str, data: &str) -> Result<()> {
    print(&target.send(reqwest::Method::POST, &format!("/tessellations/{}/schemas/check", encode(tess)), Some(&read_json(data)?)).await?)
}

/// `hexdb schema set`: register a new version (its migration runs in the background).
pub async fn schema_set(target: &Target<'_>, tess: &str, data: &str) -> Result<()> {
    let version = target.write(reqwest::Method::POST, &format!("/tessellations/{}/schemas", encode(tess)), Some(&read_json(data)?), &new_key()).await?.0;
    println!("Schema version {} of '{}' is current; existing documents are migrated in the background.", version["version"], tess);
    Ok(())
}
