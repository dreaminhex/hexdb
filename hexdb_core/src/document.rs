// HexDB Core Document Module
// This module defines the core data structures and functions for handling
// documents in HexDB. It includes the `Document` struct, which represents a
// document stored in a tessellation, and the `FieldValue` enum, which defines
// the possible types of field values. The module also includes functions for
// inferring field types from JSON values and for converting documents back
// to plain JSON.
//
// Type inference is lossless: every JSON value converts to a FieldValue and
// back to the same JSON value. Strings are only tagged as DateTime when they
// are valid RFC 3339 timestamps, and the original text is kept.

use base64::{engine::general_purpose, Engine as _};
use chrono::DateTime;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::collections::BTreeMap;
use ulid::Ulid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum FieldValue {
    Null,
    String(String),
    Integer(i64),
    Float(f64),
    Long(i128),
    Decimal(Decimal),
    Boolean(bool),
    /// An RFC 3339 timestamp, kept exactly as written.
    DateTime(String),
    Binary(Vec<u8>),
    Array(Vec<FieldValue>),
    Object(BTreeMap<String, FieldValue>),
}

// Field map
pub type CompactFields = BTreeMap<String, FieldValue>;

// Core document stored in a tessellation
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub id: Ulid,
    pub tessellation: String,
    pub data: CompactFields,
    pub ttl: Option<i64>, // epoch millis when the document expires
}

/// Keep only the given dotted field paths of a document's API JSON (plus
/// `id` and `_expires_at`). Missing paths are left out; nested paths keep the
/// enclosing objects.
pub fn project(json: &Value, fields: &[String]) -> Value {
    fn copy_path(from: &Value, to: &mut Map<String, Value>, path: &[&str]) {
        let Some((first, rest)) = path.split_first() else { return };
        let Some(value) = from.get(*first) else { return };
        if rest.is_empty() {
            to.insert(first.to_string(), value.clone());
            return;
        }
        if !value.is_object() {
            return;
        }
        let entry = to.entry(first.to_string()).or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(inner) = entry {
            copy_path(value, inner, rest);
        }
    }
    let mut out = Map::new();
    for key in ["id", "_expires_at"] {
        if let Some(v) = json.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    for field in fields {
        let path: Vec<&str> = field.split('.').collect();
        copy_path(json, &mut out, &path);
    }
    Value::Object(out)
}

impl Document {
    /// True if the document has a TTL that has passed.
    pub fn is_expired(&self, now_millis: i64) -> bool {
        self.ttl.is_some_and(|ttl| ttl <= now_millis)
    }

    /// The document as returned by the API: its fields as plain JSON, plus
    /// `id`, and `_expires_at` (RFC 3339) when it has a TTL.
    pub fn to_api_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("id".into(), Value::String(self.id.to_string()));
        if let Some(ttl) = self.ttl {
            if let Some(at) = DateTime::from_timestamp_millis(ttl) {
                map.insert(
                    "_expires_at".into(),
                    Value::String(at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
                );
            }
        }
        for (k, v) in &self.data {
            map.insert(k.clone(), v.to_json());
        }
        Value::Object(map)
    }

    /// The document's fields as plain JSON (without id or metadata).
    pub fn data_json(&self) -> Value {
        Value::Object(
            self.data
                .iter()
                .map(|(k, v)| (k.clone(), v.to_json()))
                .collect(),
        )
    }
}

impl FieldValue {
    /// Infer a typed value from JSON.
    pub fn from_json(value: &Value) -> FieldValue {
        match value {
            Value::Null => FieldValue::Null,
            Value::Bool(b) => FieldValue::Boolean(*b),
            Value::String(s) => {
                if DateTime::parse_from_rfc3339(s).is_ok() {
                    FieldValue::DateTime(s.clone())
                } else {
                    FieldValue::String(s.clone())
                }
            }
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    FieldValue::Integer(i)
                } else if let Some(u) = n.as_u64() {
                    FieldValue::Long(u as i128)
                } else if let Some(f) = n.as_f64() {
                    FieldValue::Float(f)
                } else {
                    FieldValue::String(n.to_string()) // unreachable without arbitrary_precision
                }
            }
            Value::Array(items) => FieldValue::Array(items.iter().map(FieldValue::from_json).collect()),
            Value::Object(map) => FieldValue::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), FieldValue::from_json(v)))
                    .collect(),
            ),
        }
    }

    /// Convert back to plain JSON.
    pub fn to_json(&self) -> Value {
        match self {
            FieldValue::Null => Value::Null,
            FieldValue::String(s) | FieldValue::DateTime(s) => Value::String(s.clone()),
            FieldValue::Integer(i) => Value::Number((*i).into()),
            FieldValue::Float(f) => Number::from_f64(*f).map(Value::Number).unwrap_or(Value::Null),
            FieldValue::Long(i) => match u64::try_from(*i) {
                Ok(u) => Value::Number(u.into()),
                Err(_) => Value::String(i.to_string()),
            },
            FieldValue::Decimal(d) => serde_json::from_str::<Number>(&d.to_string())
                .map(Value::Number)
                .unwrap_or_else(|_| Value::String(d.to_string())),
            FieldValue::Boolean(b) => Value::Bool(*b),
            FieldValue::Binary(bytes) => Value::String(general_purpose::STANDARD.encode(bytes)),
            FieldValue::Array(items) => Value::Array(items.iter().map(FieldValue::to_json).collect()),
            FieldValue::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), v.to_json()))
                    .collect::<Map<String, Value>>(),
            ),
        }
    }
}

/// Field names that are metadata in API responses and are ignored in request bodies.
pub const RESERVED_FIELDS: &[&str] = &["id", "_expires_at"];

/// True for fields that are ignored when writing documents.
pub fn is_reserved_field(name: &str) -> bool {
    RESERVED_FIELDS.contains(&name)
}

/// Infer typed fields from a JSON object. Non-object values produce no fields.
pub fn infer_fields_from_json(value: &Value) -> CompactFields {
    match value {
        Value::Object(fields) => fields
            .iter()
            .map(|(k, v)| (k.clone(), FieldValue::from_json(v)))
            .collect(),
        _ => CompactFields::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_round_trips_losslessly() {
        let input = json!({
            "title": "Test",
            "code": "abcd",
            "views": 445,
            "big": 18446744073709551615u64,
            "ratio": 0.25,
            "published": true,
            "nothing": null,
            "when": "2024-05-01T12:30:00+02:00",
            "tags": ["hexdb", "rust", 1, null],
            "author": { "name": "Ada", "nested": { "level": 2 } }
        });

        let fields = infer_fields_from_json(&input);
        let doc = Document { id: Ulid::new(), tessellation: "t".into(), data: fields, ttl: None };
        assert_eq!(doc.data_json(), input);
    }

    #[test]
    fn strings_are_not_reinterpreted() {
        let fields = infer_fields_from_json(&json!({ "name": "Test", "code": "abcd" }));
        assert_eq!(fields["name"], FieldValue::String("Test".into()));
        assert_eq!(fields["code"], FieldValue::String("abcd".into()));
    }

    #[test]
    fn types_are_inferred() {
        let fields = infer_fields_from_json(&json!({ "when": "2024-05-01T12:30:00Z", "n": 1, "f": 1.5 }));
        assert!(matches!(fields["when"], FieldValue::DateTime(_)));
        assert_eq!(fields["n"], FieldValue::Integer(1));
        assert_eq!(fields["f"], FieldValue::Float(1.5));
    }

    #[test]
    fn document_serde_round_trips() {
        let doc = Document {
            id: Ulid::new(),
            tessellation: "t".into(),
            data: infer_fields_from_json(&json!({ "a": [1, { "b": "c" }] })),
            ttl: Some(5),
        };
        let bytes = serde_json::to_vec(&doc).unwrap();
        assert_eq!(serde_json::from_slice::<Document>(&bytes).unwrap(), doc);
    }
}

#[cfg(test)]
mod api_json_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn api_json_is_plain_with_metadata() {
        let id = Ulid::new();
        let doc = Document {
            id,
            tessellation: "t".into(),
            data: infer_fields_from_json(&json!({ "title": "x", "tags": ["a"] })),
            ttl: Some(0),
        };
        assert_eq!(
            doc.to_api_json(),
            json!({ "id": id.to_string(), "_expires_at": "1970-01-01T00:00:00.000Z", "title": "x", "tags": ["a"] })
        );
    }
}
