// HexDB Core Filters
//
// A small JSON filter language shared by the REST and GraphQL APIs. A filter is
// a JSON object; every condition in it must hold.
//
//   { "status": "draft" }                         equality (shorthand for $eq)
//   { "views": { "$gte": 10, "$lt": 100 } }        operators on one field
//   { "author.name": "Ada" }                       dotted paths into objects
//   { "tags": "rust" }                             arrays match if any element matches
//   { "$or": [ { "a": 1 }, { "b": 2 } ] }           logical operators: $and, $or, $not
//
// Operators may be written with `$` or `_` (`$gte` or `_gte`); GraphQL
// literals need the `_` form because `$` marks variables there.
//
// Field operators: $eq, $ne, $gt, $gte, $lt, $lte, $in, $nin, $exists,
// $contains (substring, or array element), $startsWith, $endsWith, $not.
// `id` refers to the document ID. A null $eq also matches a missing field.

use crate::{document::Document, engine::EngineError};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::cmp::Ordering;

/// A parsed filter.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter(Node);

#[derive(Debug, Clone, PartialEq)]
enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Field { path: Vec<String>, ops: Vec<Op> },
}

#[derive(Debug, Clone, PartialEq)]
enum Op {
    Eq(Value),
    Ne(Value),
    Cmp(Ordering, bool, Value), // (direction, inclusive, value): $gt = (Greater, false)
    In(Vec<Value>),
    Nin(Vec<Value>),
    Exists(bool),
    Contains(Value),
    StartsWith(String),
    EndsWith(String),
    Not(Vec<Op>),
}

/// One sort key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SortKey {
    pub field: String,
    #[serde(default)]
    pub descending: bool,
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    EngineError::Invalid(format!("filter: {}", message.into())).into()
}

impl Filter {
    /// A filter that matches every document.
    pub fn all() -> Filter {
        Filter(Node::And(Vec::new()))
    }

    /// Parse a filter. `null` and `{}` match every document.
    pub fn parse(value: &Value) -> Result<Filter> {
        match value {
            Value::Null => Ok(Filter::all()),
            Value::Object(map) => Ok(Filter(parse_object(map)?)),
            _ => Err(invalid("expected a JSON object.")),
        }
    }

    /// True if the document matches.
    pub fn matches(&self, doc: &Document) -> bool {
        let json = doc.to_api_json();
        eval(&self.0, &json)
    }

    /// True if the filter matches everything.
    pub fn is_empty(&self) -> bool {
        matches!(&self.0, Node::And(nodes) if nodes.is_empty())
    }
}

/// Operators recognised with a `_` prefix as well as `$` (GraphQL literals
/// can't contain `$`, which marks variables there).
const OPERATORS: &[&str] = &[
    "and", "or", "not", "eq", "ne", "gt", "gte", "lt", "lte", "in", "nin", "exists", "contains", "startsWith", "endsWith",
];

/// The canonical `$name` of an operator key, or `None` for a field name.
/// Every `$` key is an operator (unknown ones are rejected later); a `_` key is
/// an operator only if it names a known operator.
fn operator(key: &str) -> Option<String> {
    if let Some(name) = key.strip_prefix('$') {
        return Some(format!("${}", name));
    }
    key.strip_prefix('_')
        .filter(|name| OPERATORS.contains(name))
        .map(|name| format!("${}", name))
}

fn parse_object(map: &Map<String, Value>) -> Result<Node> {
    let mut nodes = Vec::with_capacity(map.len());
    for (key, value) in map {
        nodes.push(match operator(key).as_deref() {
            Some(op @ ("$and" | "$or")) => {
                let Value::Array(items) = value else {
                    return Err(invalid(format!("{} expects an array of filters.", key)));
                };
                let children = items
                    .iter()
                    .map(|item| match item {
                        Value::Object(m) => parse_object(m),
                        _ => Err(invalid(format!("{} expects an array of filters.", key))),
                    })
                    .collect::<Result<Vec<_>>>()?;
                if op == "$and" { Node::And(children) } else { Node::Or(children) }
            }
            Some("$not") => match value {
                Value::Object(m) => Node::Not(Box::new(parse_object(m)?)),
                _ => return Err(invalid(format!("{} expects a filter object.", key))),
            },
            Some(_) => return Err(invalid(format!("unknown or misplaced operator {}.", key))),
            None => {
                if key.is_empty() || key.split('.').any(str::is_empty) {
                    return Err(invalid(format!("invalid field path '{}'.", key)));
                }
                Node::Field { path: key.split('.').map(String::from).collect(), ops: parse_ops(value)? }
            }
        });
    }
    Ok(if nodes.len() == 1 { nodes.pop().unwrap() } else { Node::And(nodes) })
}

/// Parse the condition for one field: a plain value (equality) or an object of operators.
fn parse_ops(value: &Value) -> Result<Vec<Op>> {
    let Value::Object(map) = value else { return Ok(vec![Op::Eq(value.clone())]) };
    let operators = map.keys().filter(|k| operator(k).is_some()).count();
    if operators == 0 {
        return Ok(vec![Op::Eq(value.clone())]); // object equality
    }
    if operators != map.len() {
        return Err(invalid("an object can't mix operators ($gt, _gt, ...) and fields."));
    }

    map.iter()
        .map(|(key, arg)| {
            let op = operator(key).unwrap_or_default();
            Ok(match op.as_str() {
                "$eq" => Op::Eq(arg.clone()),
                "$ne" => Op::Ne(arg.clone()),
                "$gt" => Op::Cmp(Ordering::Greater, false, arg.clone()),
                "$gte" => Op::Cmp(Ordering::Greater, true, arg.clone()),
                "$lt" => Op::Cmp(Ordering::Less, false, arg.clone()),
                "$lte" => Op::Cmp(Ordering::Less, true, arg.clone()),
                "$in" | "$nin" => {
                    let Value::Array(items) = arg else {
                        return Err(invalid(format!("{} expects an array.", key)));
                    };
                    if op == "$in" { Op::In(items.clone()) } else { Op::Nin(items.clone()) }
                }
                "$exists" => Op::Exists(arg.as_bool().ok_or_else(|| invalid(format!("{} expects true or false.", key)))?),
                "$contains" => Op::Contains(arg.clone()),
                "$startsWith" | "$endsWith" => {
                    let s = arg.as_str().ok_or_else(|| invalid(format!("{} expects a string.", key)))?.to_string();
                    if op == "$startsWith" { Op::StartsWith(s) } else { Op::EndsWith(s) }
                }
                "$not" => Op::Not(parse_ops(arg)?),
                _ => return Err(invalid(format!("unknown operator {}.", key))),
            })
        })
        .collect()
}

/// Look up a dotted path. Arrays along the way fan out (any element may match).
fn resolve<'a>(json: &'a Value, path: &[String]) -> Vec<&'a Value> {
    let Some((first, rest)) = path.split_first() else { return vec![json] };
    match json {
        Value::Object(map) => map.get(first).map(|v| resolve(v, rest)).unwrap_or_default(),
        Value::Array(items) => items.iter().flat_map(|item| resolve(item, path)).collect(),
        _ => Vec::new(),
    }
}

fn eval(node: &Node, json: &Value) -> bool {
    match node {
        Node::And(nodes) => nodes.iter().all(|n| eval(n, json)),
        Node::Or(nodes) => nodes.iter().any(|n| eval(n, json)),
        Node::Not(node) => !eval(node, json),
        Node::Field { path, ops } => {
            let values = resolve(json, path);
            ops.iter().all(|op| eval_op(op, &values))
        }
    }
}

/// Candidate values for comparison: each value, plus the elements of arrays.
fn candidates<'a>(values: &[&'a Value]) -> Vec<&'a Value> {
    let mut out = Vec::new();
    for v in values {
        out.push(*v);
        if let Value::Array(items) = v {
            out.extend(items.iter());
        }
    }
    out
}

fn eval_op(op: &Op, values: &[&Value]) -> bool {
    match op {
        Op::Eq(Value::Null) => values.is_empty() || values.iter().any(|v| v.is_null()),
        Op::Eq(expected) => candidates(values).iter().any(|v| json_eq(v, expected)),
        Op::Ne(expected) => !eval_op(&Op::Eq(expected.clone()), values),
        Op::Cmp(direction, inclusive, expected) => candidates(values).iter().any(|v| match compare(v, expected) {
            Some(Ordering::Equal) => *inclusive,
            Some(ord) => ord == *direction,
            None => false,
        }),
        Op::In(options) => options.iter().any(|o| eval_op(&Op::Eq(o.clone()), values)),
        Op::Nin(options) => !options.iter().any(|o| eval_op(&Op::Eq(o.clone()), values)),
        Op::Exists(should) => (!values.is_empty()) == *should,
        Op::Contains(needle) => values.iter().any(|v| match (v, needle) {
            (Value::String(s), Value::String(n)) => s.contains(n.as_str()),
            (Value::Array(items), n) => items.iter().any(|item| json_eq(item, n)),
            _ => false,
        }),
        Op::StartsWith(prefix) => values.iter().any(|v| v.as_str().is_some_and(|s| s.starts_with(prefix.as_str()))),
        Op::EndsWith(suffix) => values.iter().any(|v| v.as_str().is_some_and(|s| s.ends_with(suffix.as_str()))),
        Op::Not(ops) => !ops.iter().all(|op| eval_op(op, values)),
    }
}

/// Equality that treats 1 and 1.0 as equal.
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// Order two values of the same kind (numbers or strings). Mixed kinds don't compare.
fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64()?.partial_cmp(&y.as_f64()?),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// Order documents by sort keys. Missing and null values sort first (last when descending).
pub fn sort_documents(docs: &mut [Document], keys: &[SortKey]) {
    if keys.is_empty() {
        return;
    }
    let paths: Vec<Vec<String>> = keys.iter().map(|k| k.field.split('.').map(String::from).collect()).collect();
    let mut keyed: Vec<(Vec<Option<Value>>, Document)> = docs
        .iter()
        .map(|doc| {
            let json = doc.to_api_json();
            let values = paths.iter().map(|p| resolve(&json, p).first().map(|v| (*v).clone())).collect();
            (values, doc.clone())
        })
        .collect();

    keyed.sort_by(|(a, da), (b, db)| {
        for (i, key) in keys.iter().enumerate() {
            let ord = sort_order(a[i].as_ref(), b[i].as_ref());
            let ord = if key.descending { ord.reverse() } else { ord };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        da.id.cmp(&db.id)
    });

    for (slot, (_, doc)) in docs.iter_mut().zip(keyed) {
        *slot = doc;
    }
}

fn sort_rank(v: Option<&Value>) -> u8 {
    match v {
        None | Some(Value::Null) => 0,
        Some(Value::Bool(_)) => 1,
        Some(Value::Number(_)) => 2,
        Some(Value::String(_)) => 3,
        Some(Value::Array(_)) => 4,
        Some(Value::Object(_)) => 5,
    }
}

fn sort_order(a: Option<&Value>, b: Option<&Value>) -> Ordering {
    let (ra, rb) = (sort_rank(a), sort_rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Some(x), Some(y)) => compare(x, y).unwrap_or(Ordering::Equal),
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::infer_fields_from_json;
    use serde_json::json;
    use ulid::Ulid;

    fn doc(json: Value) -> Document {
        Document { id: Ulid::new(), tessellation: "t".into(), data: infer_fields_from_json(&json), ttl: None }
    }

    fn check(filter: Value, document: &Document) -> bool {
        Filter::parse(&filter).unwrap().matches(document)
    }

    #[test]
    fn equality_and_operators() {
        let d = doc(json!({ "status": "draft", "views": 42, "ratio": 1.0, "tags": ["rust", "db"], "author": { "name": "Ada" } }));
        assert!(check(json!({}), &d));
        assert!(check(json!({ "status": "draft" }), &d));
        assert!(!check(json!({ "status": "live" }), &d));
        assert!(check(json!({ "views": { "$gte": 42, "$lt": 100 } }), &d));
        assert!(!check(json!({ "views": { "$gt": 42 } }), &d));
        assert!(check(json!({ "ratio": 1 }), &d), "1 equals 1.0");
        assert!(check(json!({ "tags": "rust" }), &d), "arrays match any element");
        assert!(check(json!({ "tags": { "$in": ["go", "db"] } }), &d));
        assert!(check(json!({ "tags": { "$nin": ["go"] } }), &d));
        assert!(check(json!({ "author.name": "Ada" }), &d));
        assert!(check(json!({ "author": { "name": "Ada" } }), &d), "object equality");
        assert!(check(json!({ "missing": null }), &d), "null matches missing");
        assert!(check(json!({ "missing": { "$exists": false }, "views": { "$exists": true } }), &d));
        assert!(check(json!({ "status": { "$contains": "raf", "$startsWith": "dr", "$endsWith": "ft" } }), &d));
        assert!(check(json!({ "views": { "$not": { "$gt": 100 } } }), &d));
        assert!(check(json!({ "$or": [{ "status": "live" }, { "views": 42 }] }), &d));
        assert!(!check(json!({ "$and": [{ "status": "draft" }, { "views": 1 }] }), &d));
        assert!(check(json!({ "$not": { "status": "live" } }), &d));
        assert!(check(json!({ "id": d.id.to_string() }), &d));
        assert!(check(json!({ "views": { "_gte": 42 }, "_or": [{ "status": "draft" }] }), &d), "underscore operators");
        assert!(check(json!({ "author": { "_name": null } }), &d) == false, "unknown _keys are fields");
        assert!(!check(json!({ "views": { "$gt": "a" } }), &d), "mixed types don't compare");
    }

    #[test]
    fn rejects_bad_filters() {
        for bad in [json!([1]), json!({ "$bogus": 1 }), json!({ "a": { "$gt": 1, "b": 2 } }), json!({ "$or": {} }), json!({ "a..b": 1 })] {
            assert!(Filter::parse(&bad).is_err(), "{} should be rejected", bad);
        }
    }

    #[test]
    fn sorts_by_keys() {
        let mut docs = vec![
            doc(json!({ "n": 2, "s": "b" })),
            doc(json!({ "n": 1, "s": "c" })),
            doc(json!({ "s": "a" })),
            doc(json!({ "n": 2, "s": "a" })),
        ];
        sort_documents(&mut docs, &[SortKey { field: "n".into(), descending: true }, SortKey { field: "s".into(), descending: false }]);
        let order: Vec<Value> = docs.iter().map(|d| d.data_json()).collect();
        assert_eq!(
            order,
            vec![json!({ "n": 2, "s": "a" }), json!({ "n": 2, "s": "b" }), json!({ "n": 1, "s": "c" }), json!({ "s": "a" })]
        );
    }
}
