// HexDB Core Schemas: document versioning and migration
//
// A tessellation is schemaless until a schema is registered. Each schema is a
// numbered version: the fields documents may or must have, and (from version
// 2 on) the migration that turns a document of the previous version into one
// of this version. Like a schema registry, a new version must be backward
// compatible: every document valid under the previous version, once
// migrated, must be valid under the new one. HexDB checks that when the
// version is registered, so a migration can't strand data.
//
//   POST /tessellations/orders/schemas
//   {
//     "fields": {
//       "customer": { "type": "string", "required": true },
//       "total":    { "type": "number", "required": true, "min": 0 },
//       "status":   { "type": "string", "default": "new", "enum": ["new", "paid", "shipped"] }
//     },
//     "additional_fields": true,
//     "migration": [ { "rename": { "from": "amount", "to": "total" } } ]
//   }
//
// Writes are validated against the current version, missing fields with a
// default are filled in, and each document records the version it was written
// with in `_schema`. When a version with a migration is registered, HexDB
// rewrites the existing documents in the background (`GET .../schemas` shows
// the progress); until it finishes, reads can return documents of both versions.
//
// Field types: string, number, integer, boolean, object, array, any.
// Migration steps: rename {from, to}, copy {from, to}, remove "field",
// set_default {field, value}, convert {field, to: string | number | integer |
// boolean}. Fields are top-level names or dotted paths.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Field holding the schema version a document was written with.
pub const SCHEMA_FIELD: &str = "_schema";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    String,
    Number,
    Integer,
    Boolean,
    Object,
    Array,
    #[default]
    Any,
}

impl FieldType {
    fn accepts(self, value: &Value) -> bool {
        match self {
            FieldType::String => value.is_string(),
            FieldType::Number => value.is_number(),
            FieldType::Integer => value.as_f64().is_some_and(|f| f.fract() == 0.0),
            FieldType::Boolean => value.is_boolean(),
            FieldType::Object => value.is_object(),
            FieldType::Array => value.is_array(),
            FieldType::Any => true,
        }
    }

    fn name(self) -> &'static str {
        match self {
            FieldType::String => "string",
            FieldType::Number => "number",
            FieldType::Integer => "integer",
            FieldType::Boolean => "boolean",
            FieldType::Object => "object",
            FieldType::Array => "array",
            FieldType::Any => "any",
        }
    }
}

/// What one field must look like.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldRule {
    #[serde(rename = "type", default)]
    pub kind: FieldType,
    /// Must be present (and not null, unless `nullable`).
    #[serde(default)]
    pub required: bool,
    /// May be null.
    #[serde(default)]
    pub nullable: bool,
    /// Filled in when a write leaves the field out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// Allowed values.
    #[serde(default, rename = "enum", skip_serializing_if = "Option::is_none")]
    pub allowed: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// One migration step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Rename { from: String, to: String },
    Copy { from: String, to: String },
    Remove(String),
    SetDefault { field: String, value: Value },
    Convert { field: String, to: FieldType },
    /// Run a function over the documents (in batches): it gets them as
    /// `documents` and returns them migrated. `undo` names the function that
    /// reverses it, for rollbacks.
    Function {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        undo: Option<String>,
    },
}

/// A schema version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchemaVersion {
    pub version: u32,
    pub created: i64,
    pub fields: BTreeMap<String, FieldRule>,
    /// Allow fields not listed in `fields`.
    #[serde(default = "yes")]
    pub additional_fields: bool,
    /// Turns a document of the previous version into this one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub migration: Vec<Step>,
    /// Who registered it (migration functions run as this user).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created_by: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created_by_login: String,
    /// Set on a rollback: the version whose fields this one restores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restores: Option<u32>,
}

fn yes() -> bool {
    true
}

/// A schema as submitted (the version number is assigned).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaInput {
    #[serde(default)]
    pub fields: BTreeMap<String, FieldRule>,
    #[serde(default = "yes")]
    pub additional_fields: bool,
    #[serde(default)]
    pub migration: Vec<Step>,
}

/// One reason a document doesn't fit a schema.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Violation {
    pub field: String,
    pub message: String,
}

fn path(field: &str) -> Vec<&str> {
    field.split('.').collect()
}

fn get<'a>(doc: &'a Map<String, Value>, field: &str) -> Option<&'a Value> {
    let parts = path(field);
    let (last, parents) = parts.split_last()?;
    let mut node = doc;
    for p in parents {
        node = node.get(*p)?.as_object()?;
    }
    node.get(*last)
}

fn take(doc: &mut Map<String, Value>, field: &str) -> Option<Value> {
    let parts = path(field);
    let (last, parents) = parts.split_last()?;
    let mut node = doc;
    for p in parents {
        node = node.get_mut(*p)?.as_object_mut()?;
    }
    node.remove(*last)
}

fn set(doc: &mut Map<String, Value>, field: &str, value: Value) {
    let parts = path(field);
    let Some((last, parents)) = parts.split_last() else { return };
    let mut node = doc;
    for p in parents {
        let entry = node.entry(p.to_string()).or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        node = entry.as_object_mut().unwrap();
    }
    node.insert(last.to_string(), value);
}

fn convert(value: &Value, to: FieldType) -> Option<Value> {
    match (to, value) {
        (FieldType::String, Value::String(_)) => Some(value.clone()),
        (FieldType::String, Value::Null) => None,
        (FieldType::String, other) if !other.is_object() && !other.is_array() => Some(Value::String(other.to_string())),
        (FieldType::Number, Value::Number(_)) => Some(value.clone()),
        (FieldType::Number, Value::String(s)) => s.trim().parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number),
        (FieldType::Number, Value::Bool(b)) => Some(Value::from(*b as i64)),
        (FieldType::Integer, Value::Number(n)) => n.as_f64().map(|f| Value::from(f.round() as i64)),
        (FieldType::Integer, Value::String(s)) => s.trim().parse::<f64>().ok().map(|f| Value::from(f.round() as i64)),
        (FieldType::Boolean, Value::Bool(_)) => Some(value.clone()),
        (FieldType::Boolean, Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(Value::Bool(true)),
            "false" | "no" | "0" | "" => Some(Value::Bool(false)),
            _ => None,
        },
        (FieldType::Boolean, Value::Number(n)) => n.as_f64().map(|f| Value::Bool(f != 0.0)),
        (FieldType::Any, v) => Some(v.clone()),
        _ => None,
    }
}

impl Step {
    /// Apply this step to a document's fields.
    pub fn apply(&self, doc: &mut Map<String, Value>) {
        match self {
            Step::Rename { from, to } => {
                if let Some(v) = take(doc, from) {
                    set(doc, to, v);
                }
            }
            Step::Copy { from, to } => {
                if let Some(v) = get(doc, from).cloned() {
                    set(doc, to, v);
                }
            }
            Step::Remove(field) => {
                take(doc, field);
            }
            Step::SetDefault { field, value } => {
                if get(doc, field).is_none_or(Value::is_null) {
                    set(doc, field, value.clone());
                }
            }
            Step::Convert { field, to } => {
                if let Some(current) = get(doc, field).cloned() {
                    match convert(&current, *to) {
                        Some(v) => set(doc, field, v),
                        // A value that can't be converted is dropped rather than left mistyped.
                        None => {
                            take(doc, field);
                        }
                    }
                }
            }
            // Function steps run in the engine, over batches (see engine::schemas).
            Step::Function { .. } => {}
        }
    }

    fn check(&self) -> Result<()> {
        let fields: Vec<&String> = match self {
            Step::Rename { from, to } | Step::Copy { from, to } => vec![from, to],
            Step::Remove(f) | Step::SetDefault { field: f, .. } | Step::Convert { field: f, .. } => vec![f],
            Step::Function { name, undo } => {
                if name.is_empty() || undo.as_deref() == Some("") {
                    bail!("migration: a function step names its function (and optionally an undo function)");
                }
                vec![]
            }
        };
        for f in fields {
            if f.is_empty() || f == "id" || f.starts_with('_') || f.split('.').any(str::is_empty) {
                bail!("migration: '{}' isn't a field that can be migrated", f);
            }
        }
        Ok(())
    }
}

impl SchemaVersion {
    /// Check a document's fields; returns every problem.
    pub fn validate(&self, doc: &Map<String, Value>) -> Vec<Violation> {
        let mut out = Vec::new();
        let mut v = |field: &str, message: String| out.push(Violation { field: field.to_string(), message });
        for (field, rule) in &self.fields {
            match get(doc, field) {
                None => {
                    if rule.required {
                        v(field, "is required".into());
                    }
                }
                Some(Value::Null) => {
                    if rule.required && !rule.nullable {
                        v(field, "can't be null".into());
                    }
                }
                Some(value) => {
                    if !rule.kind.accepts(value) {
                        v(field, format!("must be {} {}", if rule.kind == FieldType::Integer || rule.kind == FieldType::Array || rule.kind == FieldType::Object { "an" } else { "a" }, rule.kind.name()));
                        continue;
                    }
                    if let Some(allowed) = &rule.allowed {
                        if !allowed.contains(value) {
                            v(field, format!("must be one of {}", Value::Array(allowed.clone())));
                        }
                    }
                    if let Some(n) = value.as_f64() {
                        if rule.min.is_some_and(|m| n < m) {
                            v(field, format!("must be at least {}", rule.min.unwrap()));
                        }
                        if rule.max.is_some_and(|m| n > m) {
                            v(field, format!("must be at most {}", rule.max.unwrap()));
                        }
                    }
                    let len = match value {
                        Value::String(s) => Some(s.chars().count()),
                        Value::Array(a) => Some(a.len()),
                        _ => None,
                    };
                    if let Some(len) = len {
                        if rule.min_length.is_some_and(|m| len < m) {
                            v(field, format!("must have at least {} {}", rule.min_length.unwrap(), if value.is_string() { "characters" } else { "items" }));
                        }
                        if rule.max_length.is_some_and(|m| len > m) {
                            v(field, format!("must have at most {} {}", rule.max_length.unwrap(), if value.is_string() { "characters" } else { "items" }));
                        }
                    }
                }
            }
        }
        if !self.additional_fields {
            for key in doc.keys().filter(|k| *k != "id" && !k.starts_with('_')) {
                if !self.fields.keys().any(|f| f == key || f.starts_with(&format!("{}.", key))) {
                    v(key, "isn't in the schema (additional_fields is false)".into());
                }
            }
        }
        out
    }

    /// Fill in missing fields that have defaults.
    pub fn apply_defaults(&self, doc: &mut Map<String, Value>) {
        for (field, rule) in &self.fields {
            if let Some(default) = &rule.default {
                if get(doc, field).is_none() {
                    set(doc, field, default.clone());
                }
            }
        }
    }

    /// True if this version's migration calls a function (it can then only
    /// run in the engine, which runs functions).
    pub fn has_function_steps(&self) -> bool {
        self.migration.iter().any(|s| matches!(s, Step::Function { .. }))
    }

    /// Run this version's migration on a document of the previous version
    /// (function steps are skipped here; see engine::schemas).
    pub fn migrate(&self, doc: &mut Map<String, Value>) {
        for step in &self.migration {
            step.apply(doc);
        }
        self.apply_defaults(doc);
    }
}

/// Check a new schema against the current one and number it.
pub fn new_version(current: &[SchemaVersion], input: SchemaInput) -> Result<SchemaVersion> {
    for (field, rule) in &input.fields {
        if field.is_empty() || field == "id" || field.starts_with('_') || field.split('.').any(str::is_empty) {
            bail!("'{}' can't be a schema field", field);
        }
        if let Some(default) = &rule.default {
            if !rule.kind.accepts(default) && !default.is_null() {
                bail!("{}: the default {} isn't a {}", field, default, rule.kind.name());
            }
        }
        if rule.min_length.zip(rule.max_length).is_some_and(|(a, b)| a > b) || rule.min.zip(rule.max).is_some_and(|(a, b)| a > b) {
            bail!("{}: the minimum is larger than the maximum", field);
        }
    }
    for step in &input.migration {
        step.check()?;
    }
    let previous = current.last();
    if previous.is_none() && !input.migration.is_empty() {
        bail!("The first schema version has no previous version to migrate from; leave out \"migration\".");
    }
    // A function step can change anything, so compatibility can't be proven
    // up front; the background migration validates every document instead
    // (and `/schemas/check` runs the function on existing documents first).
    let functions = input.migration.iter().any(|s| matches!(s, Step::Function { .. }));
    if let (Some(previous), false) = (previous, functions) {
        check_compatible(previous, &input)?;
    }
    Ok(SchemaVersion {
        version: previous.map(|p| p.version + 1).unwrap_or(1),
        created: chrono::Utc::now().timestamp_millis(),
        fields: input.fields,
        additional_fields: input.additional_fields,
        migration: input.migration,
        created_by: String::new(),
        created_by_login: String::new(),
        restores: None,
    })
}

/// The input for a rollback to version `to`: that version's fields, and a
/// migration that undoes every version after it (newest first), followed by
/// `extra` steps. Renames are reversed, copies removed, conversions converted
/// back and function steps run their `undo` function; removed fields and
/// defaults can't be undone (the compatibility check says if that matters,
/// and `extra` steps such as set_default can fill the gap).
pub fn rollback_input(versions: &[SchemaVersion], to: u32, extra: Vec<Step>) -> Result<SchemaInput> {
    let target = versions.iter().find(|v| v.version == to).ok_or_else(|| anyhow::anyhow!("There's no schema version {}.", to))?;
    let current = versions.last().map(|v| v.version).unwrap_or(0);
    if to >= current {
        bail!("Version {} is the current version; roll back to an earlier one.", to);
    }
    let mut steps = Vec::new();
    for version in versions.iter().rev().filter(|v| v.version > to) {
        // The version a step migrated from, for the type a conversion undoes to.
        let before = versions.iter().rev().find(|v| v.version < version.version);
        for step in version.migration.iter().rev() {
            match step {
                Step::Rename { from, to } => steps.push(Step::Rename { from: to.clone(), to: from.clone() }),
                Step::Copy { from: _, to: copied } => {
                    if !target.fields.contains_key(copied) {
                        steps.push(Step::Remove(copied.clone()));
                    }
                }
                Step::Convert { field, .. } => {
                    if let Some(kind) = before.and_then(|b| b.fields.get(field)).map(|r| r.kind).filter(|k| *k != FieldType::Any) {
                        steps.push(Step::Convert { field: field.clone(), to: kind });
                    }
                }
                Step::Function { name, undo } => match undo {
                    Some(undo) => steps.push(Step::Function { name: undo.clone(), undo: Some(name.clone()) }),
                    None => bail!(
                        "Version {}'s migration runs '{}', which has no undo function, so it can't be rolled back automatically. Register a new version with the steps you need instead.",
                        version.version,
                        name
                    ),
                },
                Step::Remove(_) | Step::SetDefault { .. } => {}
            }
        }
    }
    steps.extend(extra);
    Ok(SchemaInput { fields: target.fields.clone(), additional_fields: target.additional_fields, migration: steps })
}

/// Backward compatibility: a document valid under `previous`, migrated, must
/// be valid under `next`. Checked field by field for required fields, types
/// and allowed values.
fn check_compatible(previous: &SchemaVersion, next: &SchemaInput) -> Result<()> {
    // Where each field of the new version comes from after migration.
    let mut origin: BTreeMap<String, Option<String>> = previous.fields.keys().map(|f| (f.clone(), Some(f.clone()))).collect();
    let mut defaulted: Vec<String> = Vec::new();
    let mut converted: BTreeMap<String, FieldType> = BTreeMap::new();
    for step in &next.migration {
        match step {
            Step::Rename { from, to } => {
                let src = origin.remove(from).flatten().or(Some(from.clone()));
                origin.insert(to.clone(), src);
            }
            Step::Copy { from, to } => {
                let src = origin.get(from).cloned().flatten().or(Some(from.clone()));
                origin.insert(to.clone(), src);
            }
            Step::Remove(field) => {
                origin.remove(field);
            }
            Step::SetDefault { field, .. } => defaulted.push(field.clone()),
            Step::Convert { field, to } => {
                converted.insert(field.clone(), *to);
            }
            // Versions with function steps aren't checked here (see new_version).
            Step::Function { .. } => {}
        }
    }
    let mut problems = Vec::new();
    for (field, rule) in &next.fields {
        let source = origin.get(field).cloned().flatten();
        let old_rule = source.as_ref().and_then(|s| previous.fields.get(s));
        if rule.required && rule.default.is_none() && !defaulted.contains(field) {
            let guaranteed = old_rule.is_some_and(|r| r.required || r.default.is_some());
            if !guaranteed {
                problems.push(format!(
                    "'{}' is required, but documents of version {} may not have it: add a default, a set_default step, or a rename from a required field",
                    field, previous.version
                ));
            }
        }
        if let Some(old) = old_rule {
            if rule.kind != FieldType::Any {
                let effective = converted.get(field).copied().unwrap_or(old.kind);
                let fits = effective == rule.kind || (effective == FieldType::Integer && rule.kind == FieldType::Number);
                if !fits && effective == FieldType::Any {
                    problems.push(format!("'{}' had no type in version {} and is {} now: add a convert step", field, previous.version, rule.kind.name()));
                } else if !fits {
                    problems.push(format!(
                        "'{}' was {} in version {} and is {} now: add a convert step",
                        field,
                        effective.name(),
                        previous.version,
                        rule.kind.name()
                    ));
                }
            }
            if let (Some(new_allowed), old_allowed) = (&rule.allowed, &old.allowed) {
                let narrowed = match old_allowed {
                    Some(old_allowed) => old_allowed.iter().any(|v| !new_allowed.contains(v)),
                    None => true,
                };
                if narrowed && !converted.contains_key(field) {
                    problems.push(format!("'{}' allows fewer values than in version {}: existing documents may not fit", field, previous.version));
                }
            }
        }
    }
    if previous.additional_fields && !next.additional_fields {
        problems.push(format!(
            "Version {} allowed fields outside the schema and this version doesn't: existing documents may have others (remove them with migration steps, or keep additional_fields)",
            previous.version
        ));
    }
    if !problems.is_empty() {
        bail!("This version isn't compatible with version {}:\n- {}", previous.version, problems.join("\n- "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(v: Value) -> SchemaInput {
        serde_json::from_value(v).unwrap()
    }

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn validates_documents() {
        let schema = new_version(&[], input(json!({ "fields": {
            "name": { "type": "string", "required": true, "min_length": 2 },
            "age": { "type": "integer", "min": 0 },
            "role": { "type": "string", "enum": ["a", "b"], "default": "a" },
            "address.city": { "type": "string" },
        }, "additional_fields": false }))).unwrap();
        assert_eq!(schema.version, 1);
        assert!(schema.validate(&obj(json!({ "name": "Ada", "age": 36, "address": { "city": "London" } }))).is_empty());
        let problems = schema.validate(&obj(json!({ "age": 1.5, "role": "c", "extra": 1, "address": { "city": 7 } })));
        let fields: Vec<&str> = problems.iter().map(|p| p.field.as_str()).collect();
        assert_eq!(fields, ["address.city", "age", "name", "role", "extra"], "{:?}", problems);
        let mut doc = obj(json!({ "name": "Bo" }));
        schema.apply_defaults(&mut doc);
        assert_eq!(doc["role"], "a");
    }

    #[test]
    fn new_versions_must_be_compatible() {
        let v1 = new_version(&[], input(json!({ "fields": { "amount": { "type": "string", "required": true }, "note": { "type": "string" } } }))).unwrap();
        // A new required field without a default is refused.
        let err = new_version(std::slice::from_ref(&v1), input(json!({ "fields": { "amount": { "type": "string", "required": true }, "currency": { "type": "string", "required": true } } }))).unwrap_err();
        assert!(err.to_string().contains("currency"), "{}", err);
        // A type change without a convert step is refused.
        let err = new_version(std::slice::from_ref(&v1), input(json!({ "fields": { "amount": { "type": "number", "required": true } } }))).unwrap_err();
        assert!(err.to_string().contains("convert"), "{}", err);
        // Rename + convert + default: compatible, and it migrates documents.
        let v2 = new_version(
            std::slice::from_ref(&v1),
            input(json!({
                "fields": { "total": { "type": "number", "required": true }, "currency": { "type": "string", "required": true, "default": "USD" } },
                "migration": [ { "rename": { "from": "amount", "to": "total" } }, { "convert": { "field": "total", "to": "number" } }, { "remove": "note" } ],
            })),
        )
        .unwrap();
        assert_eq!(v2.version, 2);
        let mut doc = obj(json!({ "amount": "12.50", "note": "x" }));
        v2.migrate(&mut doc);
        assert_eq!(Value::Object(doc.clone()), json!({ "total": 12.5, "currency": "USD" }));
        assert!(v2.validate(&doc).is_empty());
        // The first version can't have a migration; steps can't touch id.
        assert!(new_version(&[], input(json!({ "migration": [ { "remove": "x" } ] }))).is_err());
        assert!(new_version(std::slice::from_ref(&v1), input(json!({ "fields": { "amount": { "type": "string", "required": true } }, "migration": [ { "remove": "id" } ] }))).is_err());
    }

    #[test]
    fn conversions() {
        assert_eq!(convert(&json!("42"), FieldType::Integer), Some(json!(42)));
        assert_eq!(convert(&json!("yes"), FieldType::Boolean), Some(json!(true)));
        assert_eq!(convert(&json!(3), FieldType::String), Some(json!("3")));
        assert_eq!(convert(&json!("abc"), FieldType::Number), None);
    }
}
