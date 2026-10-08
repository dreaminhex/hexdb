// HexDB Core Query Advisor
//
// HexDB keeps a summary of the queries each tessellation receives: their
// shape (which fields they test for equality or ranges, sort by, or search
// as text), how often they run, how many documents they read and how many
// they return, and which indexes they used. `GET
// /tessellations/{name}/advice` turns that into suggestions:
//
//   * an index for a frequent query that reads many more documents than it
//     returns (equality fields first, then one range field, as the planner
//     uses them), or one that sorts without an index;
//   * a text index for `$text` searches without one;
//   * dropping indexes no query has used, which only cost write time.
//
// With `?ai=true` and an Anthropic API key (`[ai]` in hexdb.toml), the
// shapes, the existing indexes and the documents' field names and types (no
// values) are also sent to Claude for a second opinion, returned separately.
// Shapes are kept per hex, saved with the metrics history
// (`query-stats.hxe`, encrypted) so they survive restarts; shapes not seen for
// 30 days are dropped when they're loaded.

use crate::{
    engine::HexDBEngine,
    filter::Filter,
    index::{IndexDef, IndexKind},
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Shapes kept per tessellation (the least used are forgotten first).
const MAX_SHAPES: usize = 200;
/// Saved shapes older than this are dropped when loaded.
const SHAPE_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// What a filter (and sort) asks of the fields, without the values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Shape {
    /// Fields compared for equality ($eq, $in).
    pub equality: BTreeSet<String>,
    /// Fields compared by range ($gt, $lt, ...) or prefix ($startsWith).
    pub range: BTreeSet<String>,
    /// Fields tested in ways no index helps with ($ne, $exists, $contains, inside $or/$not...).
    pub other: BTreeSet<String>,
    /// Uses `$text`.
    pub text: bool,
    pub sort: Vec<String>,
}

impl Shape {
    pub fn of(filter: &Filter, sort: &[crate::filter::SortKey]) -> Shape {
        let mut shape = filter.shape();
        shape.sort = sort.iter().map(|k| format!("{}{}", if k.descending { "-" } else { "" }, k.field)).collect();
        shape
    }

    pub fn is_empty(&self) -> bool {
        self.equality.is_empty() && self.range.is_empty() && self.other.is_empty() && !self.text && self.sort.is_empty()
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.equality.is_empty() {
            parts.push(format!("{} equal to a value", self.equality.iter().cloned().collect::<Vec<_>>().join(" and ")));
        }
        if !self.range.is_empty() {
            parts.push(format!("{} in a range", self.range.iter().cloned().collect::<Vec<_>>().join(" and ")));
        }
        if self.text {
            parts.push("a $text search".into());
        }
        if !self.other.is_empty() {
            parts.push(format!("other tests on {}", self.other.iter().cloned().collect::<Vec<_>>().join(", ")));
        }
        if !self.sort.is_empty() {
            parts.push(format!("sorted by {}", self.sort.join(", ")));
        }
        parts.join("; ")
    }
}

/// Totals for one shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShapeStats {
    pub shape: Shape,
    pub count: u64,
    pub scanned: u64,
    pub returned: u64,
    pub millis: f64,
    pub indexes: BTreeSet<String>,
    pub last_seen: i64,
}

/// Recorded shapes per tessellation.
#[derive(Default)]
pub struct QueryStats(std::sync::Mutex<HashMap<String, HashMap<Shape, ShapeStats>>>);

impl QueryStats {
    pub fn record(&self, tess: &str, shape: Shape, scanned: usize, returned: usize, millis: f64, indexes: &[String]) {
        if shape.is_empty() {
            return;
        }
        let mut all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let shapes = all.entry(tess.to_string()).or_default();
        if shapes.len() >= MAX_SHAPES && !shapes.contains_key(&shape) {
            if let Some(least) = shapes.iter().min_by_key(|(_, s)| (s.count, s.last_seen)).map(|(k, _)| k.clone()) {
                shapes.remove(&least);
            }
        }
        let stats = shapes.entry(shape.clone()).or_insert_with(|| ShapeStats { shape, ..Default::default() });
        stats.count += 1;
        stats.scanned += scanned as u64;
        stats.returned += returned as u64;
        stats.millis += millis;
        stats.indexes.extend(indexes.iter().cloned());
        stats.last_seen = chrono::Utc::now().timestamp_millis();
    }

    pub fn shapes(&self, tess: &str) -> Vec<ShapeStats> {
        let all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut list: Vec<ShapeStats> = all.get(tess).map(|m| m.values().cloned().collect()).unwrap_or_default();
        list.sort_by(|a, b| b.count.cmp(&a.count).then(b.scanned.cmp(&a.scanned)));
        list
    }

    pub fn forget(&self, tess: &str) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(tess);
    }

    /// Every tessellation's shapes, for saving.
    pub(crate) fn snapshot(&self) -> HashMap<String, Vec<ShapeStats>> {
        let all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        all.iter().map(|(tess, shapes)| (tess.clone(), shapes.values().cloned().collect())).collect()
    }

    /// Restore saved shapes, dropping those not seen recently.
    pub(crate) fn restore(&self, saved: HashMap<String, Vec<ShapeStats>>) {
        let cutoff = chrono::Utc::now().timestamp_millis() - SHAPE_RETENTION_MS;
        let mut all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for (tess, shapes) in saved {
            let kept: HashMap<Shape, ShapeStats> = shapes.into_iter().filter(|s| s.last_seen >= cutoff).map(|s| (s.shape.clone(), s)).collect();
            if !kept.is_empty() {
                all.insert(tess, kept);
            }
        }
    }
}

/// One suggestion.
#[derive(Debug, Clone, Serialize)]
pub struct Suggestion {
    /// "create_index" or "drop_index".
    pub action: String,
    /// For create_index: the index to create.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<IndexDef>,
    /// For drop_index: its name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// "high", "medium" or "low".
    pub impact: String,
    pub reason: String,
    /// "rules" or "ai".
    pub source: String,
}

fn avg(total: u64, count: u64) -> f64 {
    if count == 0 {
        0.0
    } else {
        total as f64 / count as f64
    }
}

/// The rule-based suggestions for a tessellation.
pub fn suggest(shapes: &[ShapeStats], indexes: &[crate::index::IndexInfo], documents: usize) -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = Vec::new();
    let mut proposed: BTreeSet<Vec<String>> = BTreeSet::new();
    let field_indexes: Vec<&IndexDef> = indexes.iter().map(|i| &i.def).filter(|d| d.kind == IndexKind::Field).collect();
    let has_text_index = indexes.iter().any(|i| i.def.kind == IndexKind::Text);
    // An index already serves these fields if its own fields start with them.
    let covered = |fields: &[String]| field_indexes.iter().any(|d| d.fields.starts_with(fields));

    for s in shapes {
        let scanned = avg(s.scanned, s.count);
        let returned = avg(s.returned, s.count).max(1.0);
        let wasteful = scanned >= 50.0 && scanned / returned >= 5.0;
        let impact = if s.count >= 10 && scanned / returned >= 20.0 {
            "high"
        } else if s.count >= 3 {
            "medium"
        } else {
            "low"
        };
        if s.shape.text && !has_text_index && s.count >= 2 {
            if proposed.insert(vec!["$text".into()]) {
                out.push(Suggestion {
                    action: "create_index".into(),
                    index: None,
                    name: None,
                    impact: impact.into(),
                    reason: format!(
                        "{} $text searches read {:.0} documents each on average; a text index on the searched fields answers them directly. Choose the fields (and an analyzer) on the indexes page.",
                        s.count, scanned
                    ),
                    source: "rules".into(),
                });
            }
            continue;
        }
        // Equality fields first, then one range field: the order the planner uses.
        let mut fields: Vec<String> = s.shape.equality.iter().cloned().collect();
        if let Some(range) = s.shape.range.iter().next() {
            fields.push(range.clone());
        }
        fields.truncate(4);
        if fields.is_empty() {
            // Sorting without a filter: an index on the sort key serves pages in order.
            if let [key] = s.shape.sort.as_slice() {
                let field = key.trim_start_matches('-').to_string();
                if !covered(std::slice::from_ref(&field)) && s.count >= 3 && documents >= 100 && proposed.insert(vec![field.clone()]) {
                    out.push(Suggestion {
                        action: "create_index".into(),
                        index: Some(IndexDef { name: String::new(), kind: IndexKind::Field, fields: vec![field.clone()], unique: false, analyzer: None }),
                        name: None,
                        impact: impact.into(),
                        reason: format!("{} queries sort all {} documents by {}; with an index they read only the page they return.", s.count, documents, field),
                        source: "rules".into(),
                    });
                }
            }
            continue;
        }
        if wasteful && s.count >= 2 && !covered(&fields) && proposed.insert(fields.clone()) {
            out.push(Suggestion {
                action: "create_index".into(),
                index: Some(IndexDef { name: String::new(), kind: IndexKind::Field, fields: fields.clone(), unique: false, analyzer: None }),
                name: None,
                impact: impact.into(),
                reason: format!(
                    "{} queries ({}) read {:.0} documents to return {:.0} on average. An index on {} narrows them to the matches.",
                    s.count,
                    s.shape.describe(),
                    scanned,
                    avg(s.returned, s.count),
                    fields.join(" + ")
                ),
                source: "rules".into(),
            });
        }
    }

    // Indexes nothing used, once there's enough traffic to judge.
    let total: u64 = shapes.iter().map(|s| s.count).sum();
    if total >= 50 {
        let used: BTreeSet<&String> = shapes.iter().flat_map(|s| s.indexes.iter()).collect();
        for def in indexes.iter().map(|i| &i.def).filter(|d| !d.unique && !used.contains(&d.name)) {
            out.push(Suggestion {
                action: "drop_index".into(),
                index: None,
                name: Some(def.name.clone()),
                impact: "low".into(),
                reason: format!("None of the last {} queries used index '{}'; it still costs time on every write. Keep it if rarer queries need it.", total, def.name),
                source: "rules".into(),
            });
        }
    }
    let rank = |s: &Suggestion| match s.impact.as_str() {
        "high" => 0,
        "medium" => 1,
        _ => 2,
    };
    out.sort_by_key(rank);
    out
}

/// AI settings (`[ai]` in hexdb.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiConfig {
    /// Environment variable holding the Anthropic API key.
    #[serde(default = "default_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
}

fn default_key_env() -> String {
    "ANTHROPIC_API_KEY".into()
}

fn default_model() -> String {
    "claude-sonnet-5-5".into()
}

fn default_base_url() -> String {
    "https://api.anthropic.com".into()
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig { api_key_env: default_key_env(), model: default_model(), base_url: default_base_url() }
    }
}

/// Field names and JSON types seen in a sample of documents (no values).
fn field_types(docs: &[crate::document::Document]) -> BTreeMap<String, BTreeSet<&'static str>> {
    fn walk(prefix: &str, value: &Value, out: &mut BTreeMap<String, BTreeSet<&'static str>>) {
        if let Value::Object(map) = value {
            for (k, v) in map {
                if k == "id" && prefix.is_empty() {
                    continue;
                }
                let path = if prefix.is_empty() { k.clone() } else { format!("{}.{}", prefix, k) };
                let kind = match v {
                    Value::Null => "null",
                    Value::Bool(_) => "boolean",
                    Value::Number(_) => "number",
                    Value::String(_) => "string",
                    Value::Array(_) => "array",
                    Value::Object(_) => "object",
                };
                out.entry(path.clone()).or_default().insert(kind);
                if prefix.matches('.').count() < 3 {
                    walk(&path, v, out);
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    for doc in docs {
        walk("", &doc.to_api_json(), &mut out);
    }
    out
}

/// Ask Claude for index suggestions. Only shapes, index definitions and field
/// names and types are sent; no document values.
pub async fn ai_suggestions(engine: &HexDBEngine, tess: &str, shapes: &[ShapeStats]) -> Result<Vec<Suggestion>> {
    let config = &engine.config.ai;
    let key = std::env::var(&config.api_key_env).map_err(|_| anyhow!("set {} in the server's environment to use AI advice", config.api_key_env))?;
    let sample = engine.list_documents(tess, None, 50).await?.documents;
    let indexes: Vec<Value> = engine.list_indexes(tess).iter().map(|i| json!({ "name": i.def.name, "kind": i.def.kind, "fields": i.def.fields, "unique": i.def.unique })).collect();
    let shapes: Vec<Value> = shapes
        .iter()
        .take(40)
        .map(|s| json!({ "shape": s.shape, "count": s.count, "avg_scanned": avg(s.scanned, s.count), "avg_returned": avg(s.returned, s.count), "avg_ms": s.millis / s.count.max(1) as f64, "indexes_used": s.indexes }))
        .collect();
    let context = json!({
        "tessellation": tess,
        "documents": engine.count_documents(tess).await?,
        "fields": field_types(&sample),
        "indexes": indexes,
        "query_shapes": shapes,
    });
    let system = "You are a database performance advisor for HexDB, a document database. Field indexes cover one or more \
        fields (equality on all fields, or equality on leading fields plus a range or prefix on the next; arrays index each element; \
        one optional unique flag). A tessellation can have one text index (for $text) over several string fields, with an analyzer: \
        standard, simple, whitespace, keyword, english, ngram or autocomplete. Every index costs time on every write. Suggest only \
        indexes the observed queries would use, plus drops of clearly useless indexes. Reply with only a JSON array of objects: \
        {\"action\": \"create_index\" | \"drop_index\", \"kind\": \"field\" | \"text\", \"fields\": [...], \"analyzer\": optional, \
        \"name\": index name for drops, \"impact\": \"high\" | \"medium\" | \"low\", \"reason\": one or two sentences}.";
    let body = json!({
        "model": config.model,
        "max_tokens": 1500,
        "system": system,
        "messages": [{ "role": "user", "content": format!("Workload summary:\n{}", serde_json::to_string_pretty(&context)?) }],
    });
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?
        .post(format!("{}/v1/messages", config.base_url.trim_end_matches('/')))
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .context("calling the Claude API")?;
    if !response.status().is_success() {
        bail!("the Claude API returned {}: {}", response.status(), response.text().await.unwrap_or_default());
    }
    let reply: Value = response.json().await?;
    let text = reply["content"].as_array().and_then(|c| c.iter().find_map(|p| p["text"].as_str())).unwrap_or("[]");
    let start = text.find('[').unwrap_or(0);
    let end = text.rfind(']').map(|e| e + 1).unwrap_or(text.len());
    let items: Vec<Value> = serde_json::from_str(&text[start..end]).context("Claude's answer wasn't the expected JSON")?;
    let mut out = Vec::new();
    for item in items {
        let impact = item["impact"].as_str().filter(|i| ["high", "medium", "low"].contains(i)).unwrap_or("medium").to_string();
        let reason = item["reason"].as_str().unwrap_or_default().chars().take(600).collect::<String>();
        match item["action"].as_str() {
            Some("create_index") => {
                let fields: Vec<String> = item["fields"].as_array().map(|f| f.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
                let kind = if item["kind"] == "text" { IndexKind::Text } else { IndexKind::Field };
                let analyzer = item["analyzer"].as_str().map(String::from).filter(|_| kind == IndexKind::Text);
                // Only well-formed definitions are passed on.
                if let Ok(def) = (IndexDef { name: String::new(), kind, fields, unique: false, analyzer }).validated() {
                    out.push(Suggestion { action: "create_index".into(), index: Some(def), name: None, impact, reason, source: "ai".into() });
                }
            }
            Some("drop_index") => {
                if let Some(name) = item["name"].as_str().filter(|n| engine.list_indexes(tess).iter().any(|i| i.def.name == *n)) {
                    out.push(Suggestion { action: "drop_index".into(), index: None, name: Some(name.to_string()), impact, reason, source: "ai".into() });
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::SortKey;

    fn shape(filter: Value, sort: &[&str]) -> Shape {
        let sort: Vec<SortKey> = sort.iter().map(|s| SortKey { field: s.trim_start_matches('-').into(), descending: s.starts_with('-') }).collect();
        Shape::of(&Filter::parse(&filter).unwrap(), &sort)
    }

    #[test]
    fn shapes_ignore_values() {
        let a = shape(json!({ "status": "live", "views": { "$gte": 10 } }), &["-views"]);
        let b = shape(json!({ "status": "draft", "views": { "$lt": 99 } }), &["-views"]);
        assert_eq!(a, b);
        assert_eq!(a.equality.iter().collect::<Vec<_>>(), ["status"]);
        assert_eq!(a.range.iter().collect::<Vec<_>>(), ["views"]);
        let or = shape(json!({ "$or": [{ "a": 1 }, { "b": 2 }] }), &[]);
        assert!(or.equality.is_empty() && or.other.contains("a"));
        assert!(shape(json!({ "$text": "hex" }), &[]).text);
    }

    #[test]
    fn suggests_indexes_for_wasteful_queries_and_drops_unused_ones() {
        let stats = QueryStats::default();
        for _ in 0..12 {
            stats.record("posts", shape(json!({ "status": "live", "views": { "$gt": 5 } }), &[]), 1000, 10, 3.0, &[]);
        }
        stats.record("posts", shape(json!({ "author": "ada" }), &[]), 1000, 900, 2.0, &[]);
        let unused = crate::index::IndexInfo {
            def: IndexDef { name: "old".into(), kind: IndexKind::Field, fields: vec!["legacy".into()], unique: false, analyzer: None },
            documents: 1000,
            keys: 5,
            ready: true,
        };
        for _ in 0..40 {
            stats.record("posts", shape(json!({ "$text": "rust" }), &[]), 1000, 3, 5.0, &[]);
        }
        let suggestions = suggest(&stats.shapes("posts"), &[unused], 1000);
        let create: Vec<&Suggestion> = suggestions.iter().filter(|s| s.action == "create_index").collect();
        assert!(create.iter().any(|s| s.index.as_ref().is_some_and(|d| d.fields == ["status", "views"]) && s.impact == "high"), "{:#?}", suggestions);
        assert!(create.iter().any(|s| s.index.is_none() && s.reason.contains("text index")), "text index suggested");
        assert!(!create.iter().any(|s| s.index.as_ref().is_some_and(|d| d.fields == ["author"])), "returning most documents isn't wasteful");
        assert!(suggestions.iter().any(|s| s.action == "drop_index" && s.name.as_deref() == Some("old")));
    }
}
