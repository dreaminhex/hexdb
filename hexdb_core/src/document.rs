// HexDB Core Document Module
// This module defines the core data structures and functions for handling
// documents in HexDB. It includes the `Document` struct, which represents a
// document stored in a tessellation, and the `FieldValue` enum, which defines
// the possible types of field values. The module also includes functions for
// inferring field types from JSON values and for serializing and deserializing
// documents to and from JSON format.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use ulid::Ulid;
use rust_decimal::Decimal;
use std::str::FromStr;
use base64::{engine::general_purpose, Engine as _};
use chrono::DateTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum FieldValue {
    String(String),
    Integer(i64),
    Float(f64),
    Long(i128),
    Decimal(Decimal), 
    Boolean(bool),
    DateTime(i64),
    Binary(Vec<u8>),
}

// Field map
pub type CompactFields = HashMap<String, FieldValue>;

// Core document stored in a tessellation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: Ulid,
    pub tessellation: String,
    pub data: CompactFields,
    pub ttl: Option<i64>, // epoch millis when the document expires
}

// Infer fields from JSON values
pub fn infer_fields_from_json(value: &Value) -> HashMap<String, FieldValue> {
    let mut map = HashMap::new();

    if let Value::Object(fields) = value {
        for (key, val) in fields {
            let inferred = match val {
                Value::String(s) => {
                    // Try ISO 8601 datetime
                    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                        FieldValue::DateTime(dt.timestamp_millis())
                    }
                    // Try base64 binary
                    else if let Ok(bytes) = general_purpose::STANDARD.decode(s) {
                        FieldValue::Binary(bytes)
                    }
                    // Fallback to string
                    else {
                        FieldValue::String(s.clone())
                    }
                }
                Value::Number(n) => {
                    // Try i64/i128 first
                    if let Some(i) = n.as_i64() {
                        FieldValue::Integer(i)
                    } else if let Ok(big) = n.to_string().parse::<i128>() {
                        FieldValue::Long(big)
                    } else if let Some(f) = n.as_f64() {
                        FieldValue::Float(f)
                    } else if let Ok(d) = Decimal::from_str(&n.to_string()) {
                        FieldValue::Decimal(d)
                    } else {
                        FieldValue::String(n.to_string()) // Fallback
                    }
                }
                Value::Bool(b) => FieldValue::Boolean(*b),
                Value::Null => continue, // Skip nulls
                _ => FieldValue::String(val.to_string()),
            };

            map.insert(key.clone(), inferred);
        }
    }

    map
}

