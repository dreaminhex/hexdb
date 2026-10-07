// HexDB Core Aggregations
//
// Group the documents that match a filter and compute summaries per group.
// Shared by the REST (`POST /{tessellation}/_aggregate`) and GraphQL
// (`aggregate`) APIs.
//
//   {
//     "filter":     { "status": "published" },
//     "group_by":   ["author.name"],
//     "aggregates": {
//       "posts":     { "$count": "*" },
//       "views":     { "$sum": "views" },
//       "avg_views": { "$avg": "views" },
//       "first":     { "$min": "published_at" },
//       "tags":      { "$countDistinct": "tags" }
//     },
//     "sort":  [{ "field": "views", "descending": true }],
//     "limit": 10
//   }
//
// Each result row holds the group-by fields (under their dotted names) and one
// column per aggregate. Without group_by there is a single row covering every
// matching document. Operators may be written with `$` or `_`.

use crate::{
    document::Document,
    engine::EngineError,
    filter::{resolve, sort_order, Filter, SortKey},
};
use anyhow::Result;
use serde::Serialize;
use serde_json::{Map, Number, Value};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};

/// Most groups an aggregation may produce (before limit/offset).
pub const MAX_GROUPS: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AggregateOp {
    /// Documents in the group (`"*"`), or documents where the field has a non-null value.
    Count,
    /// Distinct non-null values of the field (array elements count individually).
    CountDistinct,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggregateOp {
    fn parse(key: &str) -> Option<AggregateOp> {
        let name = key.strip_prefix('$').or_else(|| key.strip_prefix('_'))?;
        Some(match name {
            "count" => AggregateOp::Count,
            "countDistinct" | "count_distinct" => AggregateOp::CountDistinct,
            "sum" => AggregateOp::Sum,
            "avg" => AggregateOp::Avg,
            "min" => AggregateOp::Min,
            "max" => AggregateOp::Max,
            _ => return None,
        })
    }
}

/// One output column: `name = op(field)`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AggregateSpec {
    pub name: String,
    pub op: AggregateOp,
    /// Dotted field path; `None` only for `$count: "*"`.
    pub field: Option<String>,
}

/// A parsed aggregation request.
#[derive(Debug, Clone)]
pub struct Aggregation {
    pub filter: Filter,
    pub group_by: Vec<String>,
    pub aggregates: Vec<AggregateSpec>,
    /// Sort keys over output columns. Default: group-by fields ascending.
    pub sort: Vec<SortKey>,
    pub offset: usize,
    pub limit: usize,
}

/// Aggregation results.
#[derive(Debug, Clone, Serialize)]
pub struct AggregateResult {
    pub rows: Vec<Map<String, Value>>,
    /// Number of groups before offset/limit.
    pub total_groups: usize,
    /// Number of documents that matched the filter.
    pub matched: usize,
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    EngineError::Invalid(format!("aggregate: {}", message.into())).into()
}

/// Parse `{"alias": {"$op": "field"}, ...}`. An empty or missing object means `{"count": {"$count": "*"}}`.
pub fn parse_aggregates(value: &Value) -> Result<Vec<AggregateSpec>> {
    let map = match value {
        Value::Null => return Ok(vec![AggregateSpec { name: "count".into(), op: AggregateOp::Count, field: None }]),
        Value::Object(map) if map.is_empty() => {
            return Ok(vec![AggregateSpec { name: "count".into(), op: AggregateOp::Count, field: None }])
        }
        Value::Object(map) => map,
        _ => return Err(invalid("aggregates must be an object like {\"total\": {\"$sum\": \"price\"}}.")),
    };
    let mut specs = Vec::with_capacity(map.len());
    for (name, definition) in map {
        if name.is_empty() {
            return Err(invalid("aggregate names can't be empty."));
        }
        let Value::Object(def) = definition else {
            return Err(invalid(format!("'{}' must be an object like {{\"$sum\": \"price\"}}.", name)));
        };
        if def.len() != 1 {
            return Err(invalid(format!("'{}' must have exactly one operator.", name)));
        }
        let (key, arg) = def.iter().next().unwrap();
        let op = AggregateOp::parse(key).ok_or_else(|| {
            invalid(format!(
                "unknown operator {} in '{}'. Use $count, $countDistinct, $sum, $avg, $min, or $max.",
                key, name
            ))
        })?;
        let field = match (op, arg) {
            (AggregateOp::Count, Value::String(s)) if s == "*" => None,
            (AggregateOp::Count, Value::Bool(true) | Value::Null) => None,
            (AggregateOp::Count, Value::Object(o)) if o.is_empty() => None,
            (_, Value::String(s)) if !s.is_empty() && s != "*" => Some(s.clone()),
            _ => return Err(invalid(format!("{} in '{}' needs a field name.", key, name))),
        };
        specs.push(AggregateSpec { name: name.clone(), op, field });
    }
    Ok(specs)
}

impl Aggregation {
    /// Build an aggregation, validating that output column names are unique.
    pub fn new(
        filter: Filter,
        group_by: Vec<String>,
        aggregates: Vec<AggregateSpec>,
        sort: Vec<SortKey>,
        offset: usize,
        limit: usize,
    ) -> Result<Aggregation> {
        let mut seen = HashSet::new();
        for g in &group_by {
            if g.is_empty() {
                return Err(invalid("group_by field names can't be empty."));
            }
            if !seen.insert(g.as_str()) {
                return Err(invalid(format!("'{}' appears twice in group_by.", g)));
            }
        }
        for a in &aggregates {
            if !seen.insert(a.name.as_str()) {
                return Err(invalid(format!("'{}' is both a group_by field and an aggregate name, or is used twice.", a.name)));
            }
        }
        for s in &sort {
            if !seen.contains(s.field.as_str()) {
                return Err(invalid(format!(
                    "can't sort by '{}': sort by a group_by field or an aggregate name.",
                    s.field
                )));
            }
        }
        Ok(Aggregation { filter, group_by, aggregates, sort, offset, limit })
    }

    /// Parse a whole request body: `{"filter", "group_by", "aggregates", "sort", "offset", "limit"}`.
    pub fn from_json(body: &Value) -> Result<Aggregation> {
        let Value::Object(map) = body else {
            return Err(invalid("expected a JSON object."));
        };
        for key in map.keys() {
            if !["filter", "group_by", "groupBy", "aggregates", "sort", "offset", "limit"].contains(&key.as_str()) {
                return Err(invalid(format!("unknown key '{}'.", key)));
            }
        }
        let filter = Filter::parse(map.get("filter").unwrap_or(&Value::Null))?;
        let group_by = match map.get("group_by").or_else(|| map.get("groupBy")) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| v.as_str().map(String::from).ok_or_else(|| invalid("group_by must be a list of field names.")))
                .collect::<Result<_>>()?,
            _ => return Err(invalid("group_by must be a list of field names.")),
        };
        let aggregates = parse_aggregates(map.get("aggregates").unwrap_or(&Value::Null))?;
        let sort: Vec<SortKey> = match map.get("sort") {
            None | Some(Value::Null) => Vec::new(),
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|_| invalid("sort must be a list like [{\"field\": \"total\", \"descending\": true}]."))?,
        };
        let number = |key: &str, default: usize| -> Result<usize> {
            match map.get(key) {
                None | Some(Value::Null) => Ok(default),
                Some(v) => v.as_u64().map(|n| n as usize).ok_or_else(|| invalid(format!("{} must be a non-negative integer.", key))),
            }
        };
        let offset = number("offset", 0)?;
        let limit = number("limit", 1000)?.min(10_000);
        Aggregation::new(filter, group_by, aggregates, sort, offset, limit)
    }

    /// Run the aggregation over already-filtered documents.
    pub fn run<'a>(&self, documents: impl IntoIterator<Item = &'a Document>) -> Result<AggregateResult> {
        let group_paths: Vec<Vec<String>> = self.group_by.iter().map(|g| split(g)).collect();
        let agg_paths: Vec<Option<Vec<String>>> = self.aggregates.iter().map(|a| a.field.as_deref().map(split)).collect();

        // Group key (serialized) -> (key values, accumulators).
        let mut groups: BTreeMap<String, (Vec<Value>, Vec<Accumulator>)> = BTreeMap::new();
        let mut matched = 0;
        for doc in documents {
            matched += 1;
            let json = doc.to_api_json();
            let key: Vec<Value> = group_paths
                .iter()
                .map(|p| resolve(&json, p).first().map(|v| (*v).clone()).unwrap_or(Value::Null))
                .collect();
            let id = serde_json::to_string(&key).unwrap_or_default();
            if !groups.contains_key(&id) && groups.len() >= MAX_GROUPS {
                return Err(invalid(format!("more than {} groups; group by fewer or coarser fields.", MAX_GROUPS)));
            }
            let (_, accs) = groups
                .entry(id)
                .or_insert_with(|| (key, self.aggregates.iter().map(|a| Accumulator::new(a.op)).collect()));
            for (acc, path) in accs.iter_mut().zip(&agg_paths) {
                match path {
                    None => acc.add_document(),
                    Some(path) => acc.add_values(&resolve(&json, path)),
                }
            }
        }
        // A query without group_by always has one row, even when nothing matched.
        if self.group_by.is_empty() && groups.is_empty() {
            groups.insert(String::new(), (Vec::new(), self.aggregates.iter().map(|a| Accumulator::new(a.op)).collect()));
        }

        let mut rows: Vec<Map<String, Value>> = groups
            .into_values()
            .map(|(key, accs)| {
                let mut row = Map::new();
                for (name, value) in self.group_by.iter().zip(key) {
                    row.insert(name.clone(), value);
                }
                for (spec, acc) in self.aggregates.iter().zip(accs) {
                    row.insert(spec.name.clone(), acc.finish());
                }
                row
            })
            .collect();

        let sort: Vec<SortKey> = if self.sort.is_empty() {
            self.group_by.iter().map(|g| SortKey { field: g.clone(), descending: false }).collect()
        } else {
            self.sort.clone()
        };
        rows.sort_by(|a, b| {
            for key in &sort {
                let ord = sort_order(a.get(&key.field), b.get(&key.field));
                let ord = if key.descending { ord.reverse() } else { ord };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });

        let total_groups = rows.len();
        let rows = rows.into_iter().skip(self.offset).take(self.limit).collect();
        Ok(AggregateResult { rows, total_groups, matched })
    }
}

fn split(path: &str) -> Vec<String> {
    path.split('.').map(String::from).collect()
}

/// Values to aggregate: each resolved value, with arrays expanded one level.
fn flatten<'a>(values: &[&'a Value]) -> Vec<&'a Value> {
    let mut out = Vec::new();
    for v in values {
        match v {
            Value::Array(items) => out.extend(items.iter()),
            other => out.push(*other),
        }
    }
    out
}

fn as_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

enum Accumulator {
    Count(u64),
    CountDistinct(HashSet<String>),
    Sum { int: i128, float: f64, any_float: bool, seen: bool },
    Avg { sum: f64, n: u64 },
    Min(Option<Value>),
    Max(Option<Value>),
}

impl Accumulator {
    fn new(op: AggregateOp) -> Self {
        match op {
            AggregateOp::Count => Accumulator::Count(0),
            AggregateOp::CountDistinct => Accumulator::CountDistinct(HashSet::new()),
            AggregateOp::Sum => Accumulator::Sum { int: 0, float: 0.0, any_float: false, seen: false },
            AggregateOp::Avg => Accumulator::Avg { sum: 0.0, n: 0 },
            AggregateOp::Min => Accumulator::Min(None),
            AggregateOp::Max => Accumulator::Max(None),
        }
    }

    /// `$count: "*"`: every document counts.
    fn add_document(&mut self) {
        if let Accumulator::Count(n) = self {
            *n += 1;
        }
    }

    fn add_values(&mut self, values: &[&Value]) {
        match self {
            Accumulator::Count(n) => {
                if values.iter().any(|v| !v.is_null()) {
                    *n += 1;
                }
            }
            Accumulator::CountDistinct(set) => {
                for v in flatten(values).into_iter().filter(|v| !v.is_null()) {
                    set.insert(serde_json::to_string(v).unwrap_or_default());
                }
            }
            Accumulator::Sum { int, float, any_float, seen } => {
                for v in flatten(values) {
                    if let Value::Number(n) = v {
                        *seen = true;
                        match n.as_i64() {
                            Some(i) if !*any_float => *int += i as i128,
                            _ => {
                                if !*any_float {
                                    *any_float = true;
                                    *float = *int as f64;
                                }
                                *float += n.as_f64().unwrap_or(0.0);
                            }
                        }
                    }
                }
            }
            Accumulator::Avg { sum, n } => {
                for x in flatten(values).into_iter().filter_map(as_number) {
                    *sum += x;
                    *n += 1;
                }
            }
            Accumulator::Min(best) => {
                for v in flatten(values).into_iter().filter(|v| !v.is_null()) {
                    if best.as_ref().is_none_or(|b| sort_order(Some(v), Some(b)) == Ordering::Less) {
                        *best = Some(v.clone());
                    }
                }
            }
            Accumulator::Max(best) => {
                for v in flatten(values).into_iter().filter(|v| !v.is_null()) {
                    if best.as_ref().is_none_or(|b| sort_order(Some(v), Some(b)) == Ordering::Greater) {
                        *best = Some(v.clone());
                    }
                }
            }
        }
    }

    fn finish(self) -> Value {
        match self {
            Accumulator::Count(n) => Value::from(n),
            Accumulator::CountDistinct(set) => Value::from(set.len() as u64),
            Accumulator::Sum { int, float, any_float, seen } => {
                if !seen {
                    Value::from(0)
                } else if any_float {
                    Number::from_f64(float).map(Value::Number).unwrap_or(Value::Null)
                } else if let Ok(i) = i64::try_from(int) {
                    Value::from(i)
                } else {
                    Number::from_f64(int as f64).map(Value::Number).unwrap_or(Value::Null)
                }
            }
            Accumulator::Avg { sum, n } => {
                if n == 0 {
                    Value::Null
                } else {
                    Number::from_f64(sum / n as f64).map(Value::Number).unwrap_or(Value::Null)
                }
            }
            Accumulator::Min(v) | Accumulator::Max(v) => v.unwrap_or(Value::Null),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::infer_fields_from_json;
    use serde_json::json;
    use ulid::Ulid;

    fn docs() -> Vec<Document> {
        [
            json!({ "author": { "name": "Ada" }, "status": "live", "views": 10, "tags": ["rust", "db"] }),
            json!({ "author": { "name": "Ada" }, "status": "draft", "views": 5, "tags": ["rust"] }),
            json!({ "author": { "name": "Bo" }, "status": "live", "views": 2.5 }),
            json!({ "author": { "name": "Bo" }, "status": "live" }),
            json!({ "status": "live", "views": 1 }),
        ]
        .iter()
        .map(|j| Document { id: Ulid::new(), tessellation: "t".into(), data: infer_fields_from_json(j), ttl: None })
        .collect()
    }

    fn run(body: Value) -> AggregateResult {
        let agg = Aggregation::from_json(&body).unwrap();
        let all = docs();
        let matched: Vec<&Document> = all.iter().filter(|d| agg.filter.matches(d)).collect();
        agg.run(matched).unwrap()
    }

    #[test]
    fn whole_collection_defaults_to_count() {
        let r = run(json!({}));
        assert_eq!(r.rows, vec![json!({ "count": 5 }).as_object().unwrap().clone()]);
        assert_eq!(r.total_groups, 1);
        assert_eq!(r.matched, 5);

        let none = run(json!({ "filter": { "status": "gone" }, "aggregates": { "n": { "$count": "*" }, "avg": { "$avg": "views" } } }));
        assert_eq!(serde_json::to_value(&none.rows).unwrap(), json!([{ "n": 0, "avg": null }]));
    }

    #[test]
    fn groups_with_every_operator() {
        let r = run(json!({
            "group_by": ["author.name"],
            "aggregates": {
                "n": { "$count": "*" },
                "with_views": { "_count": "views" },
                "views": { "$sum": "views" },
                "avg": { "$avg": "views" },
                "low": { "$min": "views" },
                "high": { "$max": "views" },
                "tags": { "$countDistinct": "tags" },
            },
        }));
        assert_eq!(
            serde_json::to_value(&r.rows).unwrap(),
            json!([
                { "author.name": null, "n": 1, "with_views": 1, "views": 1, "avg": 1.0, "low": 1, "high": 1, "tags": 0 },
                { "author.name": "Ada", "n": 2, "with_views": 2, "views": 15, "avg": 7.5, "low": 5, "high": 10, "tags": 2 },
                { "author.name": "Bo", "n": 2, "with_views": 1, "views": 2.5, "avg": 2.5, "low": 2.5, "high": 2.5, "tags": 0 },
            ])
        );
    }

    #[test]
    fn filters_sorts_and_pages_groups() {
        let r = run(json!({
            "filter": { "status": "live" },
            "group_by": "author.name",
            "aggregates": { "views": { "$sum": "views" } },
            "sort": [{ "field": "views", "descending": true }],
            "limit": 2,
        }));
        assert_eq!(r.total_groups, 3);
        assert_eq!(r.matched, 4);
        assert_eq!(serde_json::to_value(&r.rows).unwrap(), json!([{ "author.name": "Ada", "views": 10 }, { "author.name": "Bo", "views": 2.5 }]));
    }

    #[test]
    fn rejects_bad_requests() {
        for body in [
            json!({ "aggregates": { "x": { "$median": "v" } } }),
            json!({ "aggregates": { "x": { "$sum": "*" } } }),
            json!({ "aggregates": { "x": { "$sum": "a", "$avg": "b" } } }),
            json!({ "group_by": ["a"], "aggregates": { "a": { "$count": "*" } } }),
            json!({ "sort": [{ "field": "nope" }] }),
            json!({ "having": {} }),
            json!({ "group_by": [1] }),
        ] {
            assert!(Aggregation::from_json(&body).is_err(), "{}", body);
        }
    }
}
