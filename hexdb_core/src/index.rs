// HexDB Core Indexes
//
// Secondary indexes speed up filtered queries, counts, and aggregations.
//
//   Field index:  one or more field paths (composite), optionally unique.
//                 Answers equality, $in, ranges ($gt/$gte/$lt/$lte), and
//                 $startsWith on its first field, and equality on all its
//                 fields together. Array values index each element.
//   Text index:   an inverted index of the words in one or more string fields.
//                 Answers $text.
//
// Every document is reachable by its ID (the primary key, a ULID that HexDB
// generates and that sorts by creation time), so no index is needed for that.
//
// Index definitions are stored in the catalog; index contents live in memory
// and are rebuilt when HexDB starts. Writes update indexes inside the commit,
// so queries always see their own writes.
//
// Indexes only narrow down candidates: the query planner returns a superset of
// the matching document IDs, and the full filter is still checked on each
// candidate. Results are therefore identical with or without an index.

use crate::{
    document::Document,
    engine::EngineError,
    filter::{resolve, strings, tokenize, Filter, Node, Op},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Bound;
use ulid::Ulid;

/// Most index keys one document may produce in one index (arrays multiply).
const MAX_KEYS_PER_DOCUMENT: usize = 1_000;
/// Most indexes per tessellation.
pub const MAX_INDEXES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum IndexKind {
    #[default]
    Field,
    Text,
}

/// An index definition, as stored in the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDef {
    /// Defaults to the field names joined with `_` (plus `_text` for text indexes).
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: IndexKind,
    /// Dotted field paths.
    pub fields: Vec<String>,
    /// Field indexes only: no two documents may share a key. Documents
    /// missing every indexed field are not checked.
    #[serde(default)]
    pub unique: bool,
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    EngineError::Invalid(message.into()).into()
}

impl IndexDef {
    /// Validate a definition; fills in a default name.
    pub fn validated(mut self) -> Result<IndexDef> {
        if self.fields.is_empty() {
            return Err(invalid("An index needs at least one field."));
        }
        if self.fields.len() > 8 {
            return Err(invalid("An index can have at most 8 fields."));
        }
        let mut seen = HashSet::new();
        for f in &self.fields {
            if f.is_empty() || f.split('.').any(str::is_empty) {
                return Err(invalid(format!("Invalid field path '{}'.", f)));
            }
            if f == "id" {
                return Err(invalid("Documents are already indexed by id."));
            }
            if !seen.insert(f) {
                return Err(invalid(format!("'{}' appears twice in the index.", f)));
            }
        }
        if self.kind == IndexKind::Text && self.unique {
            return Err(invalid("Text indexes can't be unique."));
        }
        if self.name.is_empty() {
            let suffix = if self.kind == IndexKind::Text { "_text" } else { "" };
            self.name = format!("{}{}", self.fields.join("_").replace('.', "_"), suffix);
        }
        if self.name.len() > 64 || !self.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(invalid("Index names are 1-64 letters, digits, '_' or '-'."));
        }
        Ok(self)
    }
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// One indexed value, ordered like query sorting: null < bool < number < string.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum KeyPart {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
}

impl Eq for KeyPart {}

impl PartialOrd for KeyPart {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for KeyPart {
    fn cmp(&self, other: &Self) -> Ordering {
        fn rank(k: &KeyPart) -> u8 {
            match k {
                KeyPart::Null => 0,
                KeyPart::Bool(_) => 1,
                KeyPart::Num(_) => 2,
                KeyPart::Str(_) => 3,
            }
        }
        match (self, other) {
            (KeyPart::Bool(a), KeyPart::Bool(b)) => a.cmp(b),
            (KeyPart::Num(a), KeyPart::Num(b)) => a.total_cmp(b),
            (KeyPart::Str(a), KeyPart::Str(b)) => a.cmp(b),
            _ => rank(self).cmp(&rank(other)),
        }
    }
}

impl KeyPart {
    /// The key for a scalar JSON value; `None` for arrays and objects.
    pub(crate) fn from_scalar(value: &Value) -> Option<KeyPart> {
        Some(match value {
            Value::Null => KeyPart::Null,
            Value::Bool(b) => KeyPart::Bool(*b),
            // -0.0 and 0.0 compare equal in filters; normalize for total_cmp.
            Value::Number(n) => KeyPart::Num(n.as_f64().map(|f| if f == 0.0 { 0.0 } else { f })?),
            Value::String(s) => KeyPart::Str(s.clone()),
            _ => return None,
        })
    }
}

type Key = Vec<KeyPart>;

/// Keys for one field: each scalar value (array elements expanded), or Null if missing.
fn field_keys(json: &Value, path: &[String]) -> Vec<KeyPart> {
    let values = resolve(json, path);
    if values.is_empty() {
        return vec![KeyPart::Null];
    }
    let mut out = Vec::new();
    for v in values {
        match v {
            Value::Array(items) => out.extend(items.iter().filter_map(KeyPart::from_scalar)),
            other => out.extend(KeyPart::from_scalar(other)),
        }
    }
    // Empty arrays and objects index as Null so composite keys still cover the document.
    if out.is_empty() {
        out.push(KeyPart::Null);
    }
    out.sort();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------
// Index contents
// ---------------------------------------------------------------------------

pub(crate) struct FieldIndex {
    paths: Vec<Vec<String>>,
    unique: bool,
    entries: BTreeMap<Key, BTreeSet<Ulid>>,
    by_doc: HashMap<Ulid, Vec<Key>>,
    /// Documents whose first indexed field is an array or object. Those sort
    /// as a whole, not by the elements they're indexed under, so index order
    /// isn't sort order while any exist.
    irregular: HashSet<Ulid>,
}

pub(crate) struct TextIndex {
    paths: Vec<Vec<String>>,
    words: HashMap<String, BTreeSet<Ulid>>,
    by_doc: HashMap<Ulid, Vec<String>>,
}

pub(crate) enum IndexData {
    Field(FieldIndex),
    Text(TextIndex),
}

pub(crate) struct Index {
    pub(crate) def: IndexDef,
    pub(crate) data: IndexData,
    /// False while the index is being built; the planner doesn't use it yet.
    pub(crate) ready: bool,
}

/// Index statistics for the API.
#[derive(Debug, Clone, Serialize)]
pub struct IndexInfo {
    #[serde(flatten)]
    pub def: IndexDef,
    /// Documents indexed.
    pub documents: usize,
    /// Distinct keys (field index) or words (text index).
    pub keys: usize,
    pub ready: bool,
}

fn split(path: &str) -> Vec<String> {
    path.split('.').map(String::from).collect()
}

impl Index {
    pub(crate) fn new(def: IndexDef) -> Index {
        let paths = def.fields.iter().map(|f| split(f)).collect();
        let data = match def.kind {
            IndexKind::Field => IndexData::Field(FieldIndex {
                paths,
                unique: def.unique,
                entries: BTreeMap::new(),
                by_doc: HashMap::new(),
                irregular: HashSet::new(),
            }),
            IndexKind::Text => IndexData::Text(TextIndex { paths, words: HashMap::new(), by_doc: HashMap::new() }),
        };
        Index { def, data, ready: false }
    }

    pub(crate) fn info(&self) -> IndexInfo {
        let (documents, keys) = match &self.data {
            IndexData::Field(f) => (f.by_doc.len(), f.entries.len()),
            IndexData::Text(t) => (t.by_doc.len(), t.words.len()),
        };
        IndexInfo { def: self.def.clone(), documents, keys, ready: self.ready }
    }

    /// For a single-field index: every (first key part, ID) in key order
    /// (descending keys when `descending`), IDs ascending within a key.
    pub(crate) fn ordered(&self, descending: bool) -> Option<Vec<(KeyPart, Ulid)>> {
        let IndexData::Field(f) = &self.data else { return None };
        if f.paths.len() != 1 || !f.irregular.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(f.by_doc.len());
        let mut push = |key: &Key, ids: &BTreeSet<Ulid>| {
            for id in ids {
                out.push((key[0].clone(), *id));
            }
        };
        if descending {
            f.entries.iter().rev().for_each(|(k, ids)| push(k, ids));
        } else {
            f.entries.iter().for_each(|(k, ids)| push(k, ids));
        }
        Some(out)
    }

    pub(crate) fn contains(&self, id: &Ulid) -> bool {
        match &self.data {
            IndexData::Field(f) => f.by_doc.contains_key(id),
            IndexData::Text(t) => t.by_doc.contains_key(id),
        }
    }

    /// Keys a document produces in a field index (empty for text indexes).
    fn keys_for(&self, json: &Value) -> Vec<Key> {
        let IndexData::Field(f) = &self.data else { return Vec::new() };
        let mut keys: Vec<Key> = vec![Vec::new()];
        for path in &f.paths {
            let parts = field_keys(json, path);
            let mut next = Vec::with_capacity(keys.len() * parts.len().max(1));
            for prefix in &keys {
                for part in &parts {
                    let mut key = prefix.clone();
                    key.push(part.clone());
                    next.push(key);
                    if next.len() >= MAX_KEYS_PER_DOCUMENT {
                        break;
                    }
                }
            }
            keys = next;
        }
        keys
    }

    /// Remove a document from the index.
    pub(crate) fn remove(&mut self, id: &Ulid) {
        match &mut self.data {
            IndexData::Field(f) => {
                f.irregular.remove(id);
                for key in f.by_doc.remove(id).unwrap_or_default() {
                    if let Some(ids) = f.entries.get_mut(&key) {
                        ids.remove(id);
                        if ids.is_empty() {
                            f.entries.remove(&key);
                        }
                    }
                }
            }
            IndexData::Text(t) => {
                for word in t.by_doc.remove(id).unwrap_or_default() {
                    if let Some(ids) = t.words.get_mut(&word) {
                        ids.remove(id);
                        if ids.is_empty() {
                            t.words.remove(&word);
                        }
                    }
                }
            }
        }
    }

    /// Add (or replace) a document's entries.
    pub(crate) fn insert(&mut self, doc: &Document) {
        self.remove(&doc.id);
        let json = doc.to_api_json();
        let keys = self.keys_for(&json);
        match &mut self.data {
            IndexData::Field(f) => {
                for key in &keys {
                    f.entries.entry(key.clone()).or_default().insert(doc.id);
                }
                f.by_doc.insert(doc.id, keys);
                let first = resolve(&json, &f.paths[0]);
                if first.len() > 1 || first.iter().any(|v| v.is_array() || v.is_object()) {
                    f.irregular.insert(doc.id);
                }
            }
            IndexData::Text(t) => {
                let mut texts = Vec::new();
                for path in &t.paths {
                    for v in resolve(&json, path) {
                        strings(v, &mut texts);
                    }
                }
                let words: BTreeSet<String> = texts.iter().flat_map(|s| tokenize(s)).collect();
                for w in &words {
                    t.words.entry(w.clone()).or_default().insert(doc.id);
                }
                t.by_doc.insert(doc.id, words.into_iter().collect());
            }
        }
    }

    /// For a unique index: the ID of another document that already has one of
    /// `doc`'s keys, ignoring `ignore` (documents changing in the same batch).
    /// Keys made only of nulls (missing fields) are never checked.
    pub(crate) fn unique_conflict(&self, doc: &Document, ignore: &HashSet<Ulid>) -> Option<Ulid> {
        let IndexData::Field(f) = &self.data else { return None };
        if !f.unique {
            return None;
        }
        for key in self.keys_for(&doc.to_api_json()) {
            if key.iter().all(|k| *k == KeyPart::Null) {
                continue;
            }
            if let Some(ids) = f.entries.get(&key) {
                if let Some(other) = ids.iter().find(|id| **id != doc.id && !ignore.contains(id)) {
                    return Some(*other);
                }
            }
        }
        None
    }

    /// Keys of a document in this index (for duplicate checks within a batch).
    pub(crate) fn unique_keys(&self, doc: &Document) -> Vec<Vec<String>> {
        let IndexData::Field(f) = &self.data else { return Vec::new() };
        if !f.unique {
            return Vec::new();
        }
        self.keys_for(&doc.to_api_json())
            .into_iter()
            .filter(|k| !k.iter().all(|p| *p == KeyPart::Null))
            .map(|k| k.iter().map(|p| format!("{:?}", p)).collect())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Query planning
// ---------------------------------------------------------------------------

/// The planner's result: candidate IDs (a superset of the matches) and the indexes used.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub candidates: BTreeSet<Ulid>,
    pub indexes: Vec<String>,
}

/// Candidate IDs for a filter, or `None` if no index helps (scan everything).
pub(crate) fn plan(indexes: &[Index], filter: &Filter) -> Option<Plan> {
    let mut used = Vec::new();
    let candidates = plan_node(indexes, &filter.0, &mut used)?;
    used.sort();
    used.dedup();
    Some(Plan { candidates, indexes: used })
}

fn plan_node(indexes: &[Index], node: &Node, used: &mut Vec<String>) -> Option<BTreeSet<Ulid>> {
    match node {
        Node::And(nodes) => {
            // Composite equality first: all of an index's fields pinned by $eq.
            let mut best: Option<BTreeSet<Ulid>> = composite_equality(indexes, nodes, used);
            for child in nodes {
                if let Some(ids) = plan_node(indexes, child, used) {
                    best = Some(match best {
                        None => ids,
                        Some(current) => current.intersection(&ids).copied().collect(),
                    });
                }
            }
            best
        }
        Node::Or(nodes) => {
            let mut all = BTreeSet::new();
            let mut local = Vec::new();
            for child in nodes {
                all.extend(plan_node(indexes, child, &mut local)?);
            }
            used.extend(local);
            Some(all)
        }
        Node::Not(_) => None,
        Node::Field { path, ops } => {
            let field = path.join(".");
            let index = indexes.iter().find(|i| {
                i.ready && i.def.kind == IndexKind::Field && i.def.fields.first() == Some(&field)
            })?;
            let IndexData::Field(f) = &index.data else { return None };
            let mut best: Option<BTreeSet<Ulid>> = None;
            for op in ops {
                if let Some(ids) = first_field_lookup(f, op) {
                    best = Some(match best {
                        None => ids,
                        Some(current) => current.intersection(&ids).copied().collect(),
                    });
                }
            }
            if best.is_some() {
                used.push(index.def.name.clone());
            }
            best
        }
        Node::Text { terms, .. } => {
            let index = indexes.iter().find(|i| i.ready && i.def.kind == IndexKind::Text)?;
            let IndexData::Text(t) = &index.data else { return None };
            used.push(index.def.name.clone());
            let mut result: Option<BTreeSet<Ulid>> = None;
            for term in terms {
                let ids = t.words.get(term).cloned().unwrap_or_default();
                result = Some(match result {
                    None => ids,
                    Some(current) => current.intersection(&ids).copied().collect(),
                });
            }
            Some(result.unwrap_or_default())
        }
    }
}

/// Values an `$eq` (or plain value) condition pins a field to, if it's a scalar.
fn equality_value(ops: &[Op]) -> Option<KeyPart> {
    ops.iter().find_map(|op| match op {
        // Null equality also matches missing fields, which index as Null too.
        Op::Eq(v) => KeyPart::from_scalar(v),
        _ => None,
    })
}

fn composite_equality(indexes: &[Index], nodes: &[Node], used: &mut Vec<String>) -> Option<BTreeSet<Ulid>> {
    let mut pinned: HashMap<String, KeyPart> = HashMap::new();
    for node in nodes {
        if let Node::Field { path, ops } = node {
            if let Some(part) = equality_value(ops) {
                pinned.insert(path.join("."), part);
            }
        }
    }
    let index = indexes.iter().filter(|i| i.ready && i.def.kind == IndexKind::Field && i.def.fields.len() > 1).find(|i| {
        i.def.fields.iter().all(|f| pinned.contains_key(f))
    })?;
    let IndexData::Field(f) = &index.data else { return None };
    let key: Key = index.def.fields.iter().map(|field| pinned[field].clone()).collect();
    used.push(index.def.name.clone());
    Some(f.entries.get(&key).cloned().unwrap_or_default())
}

/// IDs whose first key part satisfies `op`, or `None` if the index can't answer it.
fn first_field_lookup(f: &FieldIndex, op: &Op) -> Option<BTreeSet<Ulid>> {
    let collect = |range: &mut dyn Iterator<Item = (&Key, &BTreeSet<Ulid>)>| -> BTreeSet<Ulid> {
        let mut out = BTreeSet::new();
        for (_, ids) in range {
            out.extend(ids.iter().copied());
        }
        out
    };
    let first_eq = |part: &KeyPart| -> BTreeSet<Ulid> {
        let start: Key = vec![part.clone()];
        collect(&mut f.entries.range(start..).take_while(|(k, _)| k.first() == Some(part)))
    };
    match op {
        Op::Eq(v) => {
            let part = KeyPart::from_scalar(v)?;
            Some(first_eq(&part))
        }
        Op::In(values) => {
            let mut out = BTreeSet::new();
            for v in values {
                out.extend(first_eq(&KeyPart::from_scalar(v)?));
            }
            Some(out)
        }
        Op::Cmp(direction, inclusive, v) => {
            let part = KeyPart::from_scalar(v)?;
            if part == KeyPart::Null {
                return None;
            }
            let same_type = |k: &KeyPart| std::mem::discriminant(k) == std::mem::discriminant(&part);
            let start: Key = vec![part.clone()];
            Some(match direction {
                Ordering::Greater => collect(
                    &mut f
                        .entries
                        .range((Bound::Included(start), Bound::Unbounded))
                        .filter(|(k, _)| *inclusive || k[0] != part)
                        .take_while(|(k, _)| same_type(&k[0])),
                ),
                _ => collect(
                    &mut f
                        .entries
                        .iter()
                        .skip_while(|(k, _)| !same_type(&k[0]))
                        .take_while(|(k, _)| same_type(&k[0]) && (k[0] < part || *inclusive && k[0] == part)),
                ),
            })
        }
        Op::StartsWith(prefix) => {
            let start: Key = vec![KeyPart::Str(prefix.clone())];
            Some(collect(
                &mut f
                    .entries
                    .range(start..)
                    .take_while(|(k, _)| matches!(&k[0], KeyPart::Str(s) if s.starts_with(prefix.as_str()))),
            ))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Per-engine index registry
// ---------------------------------------------------------------------------

/// Every tessellation's indexes.
#[derive(Default)]
pub(crate) struct Indexes {
    pub(crate) by_tessellation: HashMap<String, Vec<Index>>,
}

impl Indexes {
    /// Apply a committed write to every index of its tessellation.
    pub(crate) fn apply(&mut self, tess: &str, id: &Ulid, doc: Option<&Document>) {
        if let Some(list) = self.by_tessellation.get_mut(tess) {
            for index in list {
                match doc {
                    Some(doc) => index.insert(doc),
                    None => index.remove(id),
                }
            }
        }
    }

    /// The text index fields of a tessellation, if it has a text index.
    pub(crate) fn text_fields(&self, tess: &str) -> Option<Vec<String>> {
        self.by_tessellation
            .get(tess)?
            .iter()
            .find(|i| i.def.kind == IndexKind::Text)
            .map(|i| i.def.fields.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::infer_fields_from_json;
    use serde_json::json;

    fn doc(json: Value) -> Document {
        Document { id: Ulid::new(), tessellation: "t".into(), data: infer_fields_from_json(&json), ttl: None }
    }

    fn ready(def: IndexDef, docs: &[Document]) -> Index {
        let mut index = Index::new(def.validated().unwrap());
        for d in docs {
            index.insert(d);
        }
        index.ready = true;
        index
    }

    fn field(fields: &[&str]) -> IndexDef {
        IndexDef { name: String::new(), kind: IndexKind::Field, fields: fields.iter().map(|s| s.to_string()).collect(), unique: false }
    }

    /// The planner's candidates always include every true match.
    fn check(indexes: &[Index], docs: &[Document], filter: Value, expect_index: bool) {
        let filter = Filter::parse(&filter).unwrap();
        let truth: BTreeSet<Ulid> = docs.iter().filter(|d| filter.matches(d)).map(|d| d.id).collect();
        match plan(indexes, &filter) {
            Some(p) => {
                assert!(expect_index, "unexpectedly used {:?}", p.indexes);
                assert!(truth.is_subset(&p.candidates), "{:?} missed matches", filter);
            }
            None => assert!(!expect_index, "{:?} should use an index", filter),
        }
    }

    #[test]
    fn field_index_answers_equality_ranges_and_prefixes() {
        let docs = vec![
            doc(json!({ "status": "live", "views": 10, "tags": ["a", "b"] })),
            doc(json!({ "status": "draft", "views": 5.5 })),
            doc(json!({ "status": "live", "views": "n/a" })),
            doc(json!({ "views": 0 })),
            doc(json!({ "status": "live", "views": 100, "tags": ["b"] })),
        ];
        let indexes = vec![ready(field(&["status"]), &docs), ready(field(&["views"]), &docs), ready(field(&["tags"]), &docs)];

        for (filter, uses) in [
            (json!({ "status": "live" }), true),
            (json!({ "status": null }), true),
            (json!({ "status": { "$in": ["draft", "gone"] } }), true),
            (json!({ "views": { "$gt": 5 } }), true),
            (json!({ "views": { "$gte": 10, "$lt": 100 } }), true),
            (json!({ "views": { "$lte": 5.5 } }), true),
            (json!({ "status": { "$startsWith": "dr" } }), true),
            (json!({ "tags": "b" }), true),
            (json!({ "status": "live", "views": { "$gt": 50 } }), true),
            (json!({ "$or": [{ "status": "draft" }, { "views": 100 }] }), true),
            (json!({ "$or": [{ "status": "draft" }, { "nope": 1 }] }), false),
            (json!({ "status": { "$ne": "live" } }), false),
            (json!({ "$not": { "status": "live" } }), false),
            (json!({ "other": 1 }), false),
        ] {
            check(&indexes, &docs, filter, uses);
        }

        let p = plan(&indexes, &Filter::parse(&json!({ "views": { "$gt": 5 } })).unwrap()).unwrap();
        assert_eq!(p.candidates.len(), 3, "numbers only: 5.5, 10, 100");
        assert_eq!(p.indexes, ["views"]);
    }

    #[test]
    fn composite_and_text_indexes() {
        let docs = vec![
            doc(json!({ "country": "NZ", "city": "Auckland", "bio": "Loves Rust and databases" })),
            doc(json!({ "country": "NZ", "city": "Wellington", "bio": "rust" })),
            doc(json!({ "country": "AU", "city": "Auckland", "bio": "Python" })),
        ];
        let text = IndexDef { name: String::new(), kind: IndexKind::Text, fields: vec!["bio".into()], unique: false };
        let indexes = vec![ready(field(&["country", "city"]), &docs), ready(text, &docs)];
        assert_eq!(indexes[0].def.name, "country_city");
        assert_eq!(indexes[1].def.name, "bio_text");

        let p = plan(&indexes, &Filter::parse(&json!({ "country": "NZ", "city": "Auckland" })).unwrap()).unwrap();
        assert_eq!(p.candidates, BTreeSet::from([docs[0].id]));
        let p = plan(&indexes, &Filter::parse(&json!({ "country": "NZ" })).unwrap()).unwrap();
        assert_eq!(p.candidates.len(), 2, "prefix of a composite index");
        let p = plan(&indexes, &Filter::parse(&json!({ "$text": "rust" })).unwrap()).unwrap();
        assert_eq!(p.candidates.len(), 2);
        let p = plan(&indexes, &Filter::parse(&json!({ "$text": "rust databases" })).unwrap()).unwrap();
        assert_eq!(p.candidates, BTreeSet::from([docs[0].id]));
        check(&indexes, &docs, json!({ "city": "Auckland" }), false);
    }

    #[test]
    fn maintenance_and_uniqueness() {
        let a = doc(json!({ "email": "a@x" }));
        let mut index = ready(IndexDef { unique: true, ..field(&["email"]) }, std::slice::from_ref(&a));
        let b = doc(json!({ "email": "a@x" }));
        assert_eq!(index.unique_conflict(&b, &HashSet::new()), Some(a.id));
        assert_eq!(index.unique_conflict(&b, &HashSet::from([a.id])), None, "a is changing in the same batch");
        assert_eq!(index.unique_conflict(&doc(json!({ "other": 1 })), &HashSet::new()), None, "missing fields aren't unique");

        let mut a2 = a.clone();
        a2.data = infer_fields_from_json(&json!({ "email": "new@x" }));
        index.insert(&a2);
        assert_eq!(index.unique_conflict(&b, &HashSet::new()), None, "old key removed");
        index.remove(&a.id);
        assert_eq!(index.info().documents, 0);
        assert_eq!(index.info().keys, 0);
    }

    #[test]
    fn validates_definitions() {
        assert!(field(&[]).validated().is_err());
        assert!(field(&["a", "a"]).validated().is_err());
        assert!(field(&["id"]).validated().is_err());
        assert!(field(&["a..b"]).validated().is_err());
        assert!(IndexDef { name: "bad name".into(), ..field(&["a"]) }.validated().is_err());
        assert!(IndexDef { kind: IndexKind::Text, unique: true, ..field(&["a"]) }.validated().is_err());
        assert_eq!(field(&["author.name"]).validated().unwrap().name, "author_name");
    }
}
