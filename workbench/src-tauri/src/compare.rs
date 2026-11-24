//! Schema and index compare between two lattices: which tessellations each has,
//! and where their kinds, indexes or current schemas differ. Read-only: it
//! reports, it doesn't change either side.

use anyhow::{anyhow, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// What the compare needs from one lattice, fetched over its API.
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    /// name -> (kind, indexes by name, the current schema)
    pub tessellations: BTreeMap<String, Tess>,
}

#[derive(Debug, Default, Clone)]
pub struct Tess {
    pub kind: String,
    /// name -> definition (without counts and readiness)
    pub indexes: BTreeMap<String, Value>,
    /// The current schema's fields and `additional_fields`, or None without one.
    pub schema: Option<Value>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Difference {
    pub tessellation: String,
    /// only_left, only_right, kind, index_only_left, index_only_right, index_differs, schema
    pub what: String,
    pub detail: String,
    pub left: Option<Value>,
    pub right: Option<Value>,
}

/// The index definition, as compared: everything but what the index currently holds.
fn index_definition(index: &Value) -> Value {
    let mut def = index.clone();
    if let Some(map) = def.as_object_mut() {
        for volatile in ["documents", "keys", "ready"] {
            map.remove(volatile);
        }
    }
    def
}

/// The current schema version, as compared: its rules, not its number or date.
fn schema_rules(schemas: &Value) -> Option<Value> {
    let current = schemas["current"].as_u64()?;
    let version = schemas["versions"].as_array()?.iter().find(|v| v["version"].as_u64() == Some(current))?;
    Some(serde_json::json!({ "fields": version["fields"], "additional_fields": version["additional_fields"] }))
}

pub fn compare(left: &Snapshot, right: &Snapshot) -> Vec<Difference> {
    let mut out = Vec::new();
    let names: BTreeSet<&String> = left.tessellations.keys().chain(right.tessellations.keys()).collect();
    let diff = |t: &str, what: &str, detail: String, l: Option<Value>, r: Option<Value>| Difference { tessellation: t.to_string(), what: what.into(), detail, left: l, right: r };
    for name in names {
        match (left.tessellations.get(name), right.tessellations.get(name)) {
            (Some(_), None) => out.push(diff(name, "only_left", "Only on the left.".into(), None, None)),
            (None, Some(_)) => out.push(diff(name, "only_right", "Only on the right.".into(), None, None)),
            (Some(l), Some(r)) => {
                if l.kind != r.kind {
                    out.push(diff(name, "kind", format!("A {} tessellation on the left, {} on the right.", l.kind, r.kind), Some(l.kind.clone().into()), Some(r.kind.clone().into())));
                }
                let indexes: BTreeSet<&String> = l.indexes.keys().chain(r.indexes.keys()).collect();
                for index in indexes {
                    match (l.indexes.get(index), r.indexes.get(index)) {
                        (Some(a), None) => out.push(diff(name, "index_only_left", format!("Index {} only on the left.", index), Some(a.clone()), None)),
                        (None, Some(b)) => out.push(diff(name, "index_only_right", format!("Index {} only on the right.", index), None, Some(b.clone()))),
                        (Some(a), Some(b)) if a != b => out.push(diff(name, "index_differs", format!("Index {} is defined differently.", index), Some(a.clone()), Some(b.clone()))),
                        _ => {}
                    }
                }
                if l.schema != r.schema {
                    let detail = match (&l.schema, &r.schema) {
                        (Some(_), None) => "A schema on the left only.".to_string(),
                        (None, Some(_)) => "A schema on the right only.".to_string(),
                        _ => "The current schemas differ.".to_string(),
                    };
                    out.push(diff(name, "schema", detail, l.schema.clone(), r.schema.clone()));
                }
            }
            (None, None) => {}
        }
    }
    out
}

/// Fetches what the compare needs from a lattice with an API key that can read its tessellations.
pub async fn fetch(client: &reqwest::Client, base: &str, key: &str) -> Result<Snapshot> {
    let get = |path: String| {
        let request = client.get(format!("{}{}", base.trim_end_matches('/'), path)).bearer_auth(key);
        async move {
            let response = request.send().await?;
            let status = response.status();
            let body: Value = response.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                return Err(anyhow!("{} answered {}: {}", path, status.as_u16(), body["error"]["message"].as_str().or(body["message"].as_str()).unwrap_or("")));
            }
            Ok::<Value, anyhow::Error>(body)
        }
    };
    let list = get("/tessellations".into()).await?;
    let mut snapshot = Snapshot::default();
    for t in list["tessellations"].as_array().cloned().unwrap_or_default() {
        let Some(name) = t["name"].as_str().map(str::to_string) else { continue };
        let encoded: String = name.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{:02X}", b) }).collect();
        let indexes = get(format!("/tessellations/{}/indexes", encoded)).await?;
        let schemas = get(format!("/tessellations/{}/schemas", encoded)).await?;
        let indexes = indexes["indexes"].as_array().cloned().unwrap_or_default().iter().filter_map(|i| Some((i["name"].as_str()?.to_string(), index_definition(i)))).collect();
        snapshot.tessellations.insert(name, Tess { kind: t["kind"].as_str().unwrap_or("documents").to_string(), indexes, schema: schema_rules(&schemas) });
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tess(kind: &str, indexes: &[(&str, Value)], schema: Option<Value>) -> Tess {
        Tess { kind: kind.into(), indexes: indexes.iter().map(|(n, d)| (n.to_string(), index_definition(d))).collect(), schema }
    }

    #[test]
    fn finds_every_kind_of_difference() {
        let email = json!({ "name": "by_email", "field": "email", "unique": true, "documents": 10, "keys": 10, "ready": true });
        let mut left = Snapshot::default();
        left.tessellations.insert("orders".into(), tess("documents", &[("by_email", email.clone())], Some(json!({ "fields": { "n": { "type": "number" } }, "additional_fields": true }))));
        left.tessellations.insert("old".into(), tess("documents", &[], None));
        let mut right = Snapshot::default();
        let email_right = json!({ "name": "by_email", "field": "email", "unique": false, "documents": 3, "keys": 3, "ready": true });
        right.tessellations.insert("orders".into(), tess("documents", &[("by_email", email_right), ("by_date", json!({ "name": "by_date", "field": "date" }))], None));
        right.tessellations.insert("media".into(), tess("files", &[], None));
        let what: Vec<(String, String)> = compare(&left, &right).into_iter().map(|d| (d.tessellation, d.what)).collect();
        assert_eq!(
            what,
            vec![
                ("media".into(), "only_right".into()),
                ("old".into(), "only_left".into()),
                ("orders".into(), "index_only_right".into()),
                ("orders".into(), "index_differs".into()),
                ("orders".into(), "schema".into()),
            ]
        );
    }

    #[test]
    fn counts_and_versions_dont_count_as_differences() {
        let a = json!({ "name": "i", "field": "f", "documents": 1, "keys": 1, "ready": true });
        let b = json!({ "name": "i", "field": "f", "documents": 999, "keys": 5, "ready": false });
        assert_eq!(index_definition(&a), index_definition(&b));
        let s1 = json!({ "current": 2, "versions": [{ "version": 1, "fields": {} }, { "version": 2, "created": 5, "fields": { "x": {} }, "additional_fields": false }] });
        let s2 = json!({ "current": 7, "versions": [{ "version": 7, "created": 99, "fields": { "x": {} }, "additional_fields": false }] });
        assert_eq!(schema_rules(&s1), schema_rules(&s2));
        assert_eq!(schema_rules(&json!({ "current": null, "versions": [] })), None);
    }
}
