// HexDB Core Engine: document writes
//
// Every write goes through `commit`, which applies a batch of operations
// atomically: all expected versions are checked, the batch gets consecutive
// sequence numbers, and it is logged as a single WAL record, so recovery sees
// all of it or none of it.
//
// Operations that read before writing (replace, patch, delete, update by
// filter) use optimistic concurrency: they remember the version they read and
// the commit fails if any of those documents changed in the meantime, in which
// case the operation is planned again.
//
// Idempotency: when a caller passes an idempotency key, the operation's result
// is stored as a document in the `_idempotency` system tessellation, inside the
// same atomic batch as the write. A retry with the same key and the same
// request returns the stored result instead of writing again, even after a
// crash. Records expire after 24 hours.

use super::{EngineError, HexDBEngine, MAX_WRITE_RETRIES};
use crate::{
    changes::{Change, ChangeKind},
    filter::{sort_documents, Filter, SortKey},
    catalog::validate_tessellation_name,
    document::{is_reserved_field, CompactFields, Document, FieldValue},
    hex::DocKey,
    wal::{WalOp, WalRecord},
};
use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use futures::future::BoxFuture;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use ulid::Ulid;

/// System tessellation holding idempotency records.
pub const IDEMPOTENCY_TESSELLATION: &str = "_idempotency";
/// How long idempotency records are kept.
pub const IDEMPOTENCY_TTL_MILLIS: i64 = 24 * 60 * 60 * 1000;
/// Most documents accepted in one bulk request.
pub const MAX_BULK_ITEMS: usize = 10_000;
/// Most documents one update-by-filter may modify.
pub const MAX_UPDATE_MATCHES: usize = 100_000;
const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;

/// A client-supplied idempotency key plus a fingerprint of the request it came with.
#[derive(Debug, Clone)]
pub struct IdempotencyKey {
    pub key: String,
    pub fingerprint: String,
    /// Store only document IDs and metadata, not their fields (for bulk
    /// requests whose response is a list of IDs), so a keyed 10,000-document
    /// insert doesn't write every document twice.
    pub compact: bool,
}

impl IdempotencyKey {
    /// `request` should identify the request completely (e.g. method, path,
    /// query and canonical body) so reusing a key for a different request is detected.
    pub fn new(key: &str, request: &[u8]) -> Result<Self> {
        if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_LEN {
            return Err(EngineError::Invalid(format!(
                "Idempotency keys must be 1-{} characters long.",
                MAX_IDEMPOTENCY_KEY_LEN
            ))
            .into());
        }
        if !key.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
            return Err(EngineError::Invalid("Idempotency keys must be printable ASCII.".into()).into());
        }
        Ok(IdempotencyKey { key: key.to_string(), fingerprint: blake3::hash(request).to_hex().to_string(), compact: false })
    }

    /// Keep only IDs in the stored result (see `compact`).
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

    /// Scope the key to one user, so users can't replay or block each other's keys.
    pub fn scoped_to(mut self, owner: &str) -> Self {
        self.key = format!("{}/{}", owner, self.key);
        self
    }

    fn doc_id(&self) -> Ulid {
        let hash = blake3::hash(self.key.as_bytes());
        Ulid::from_bytes(hash.as_bytes()[..16].try_into().unwrap())
    }
}

/// The result of a write, and whether it was replayed from an idempotency record.
#[derive(Debug, Clone)]
pub struct Outcome<T> {
    pub value: T,
    pub replayed: bool,
}

impl<T> Outcome<T> {
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Outcome<U> {
        Outcome { value: f(self.value), replayed: self.replayed }
    }
}

/// Result of an update by filter.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UpdateSummary {
    /// Documents that matched the filter.
    pub matched: usize,
    /// Matched documents that actually changed.
    pub modified: usize,
}

/// One page of documents.
#[derive(Debug, Clone)]
pub struct ListPage {
    pub documents: Vec<Document>,
    /// Pass as `after` to get the next page; `None` on the last page.
    pub next: Option<Ulid>,
}

/// A filtered, sorted, paged document query.
#[derive(Debug, Clone)]
pub struct DocumentQuery {
    pub filter: Filter,
    pub sort: Vec<SortKey>,
    pub offset: usize,
    pub limit: usize,
    /// Return documents with IDs after this one (ID order only).
    pub after: Option<Ulid>,
}

impl Default for DocumentQuery {
    fn default() -> Self {
        DocumentQuery { filter: Filter::all(), sort: Vec::new(), offset: 0, limit: 100, after: None }
    }
}

/// One page of query results.
#[derive(Debug, Clone)]
pub struct QueryPage {
    pub documents: Vec<Document>,
    /// Number of documents matching the filter, across all pages.
    pub total: usize,
    /// Pass as `after` for the next page (ID order only); `None` on the last page.
    pub next: Option<Ulid>,
    /// Indexes the query used (empty: every document was scanned).
    pub indexes: Vec<String>,
    /// Documents read to answer the query.
    pub scanned: usize,
}

/// One operation in an atomic commit.
pub(crate) struct BatchItem {
    pub(crate) key: DocKey,
    pub(crate) op: WalOp,
    /// If set, the commit fails unless the document's newest version has this
    /// sequence number (0 = no version exists).
    pub(crate) expected_seq: Option<u64>,
}

/// A merge-patch: `Some` sets a field, `None` removes it.
type Changes = Vec<(String, Option<FieldValue>)>;

enum Modification {
    Replace(CompactFields),
    Patch(Changes),
}

enum IdempotencyLookup {
    Absent { seq: u64 },
    /// The stored result, and the sequence number of the idempotency record.
    Replay(Value, u64),
}

pub(crate) struct InflightGuard<'a> {
    engine: &'a HexDBEngine,
    key: String,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.engine.inflight.lock().unwrap().remove(&self.key);
    }
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    EngineError::Invalid(message.into()).into()
}

fn object_fields(json: &Value, what: &str) -> Result<CompactFields> {
    let Value::Object(map) = json else {
        return Err(invalid(format!("{}: expected a JSON object.", what)));
    };
    Ok(map
        .iter()
        .filter(|(k, _)| !is_reserved_field(k))
        .map(|(k, v)| (k.clone(), FieldValue::from_json(v)))
        .collect())
}

fn patch_changes(json: &Value, what: &str) -> Result<Changes> {
    let Value::Object(map) = json else {
        return Err(invalid(format!("{}: expected a JSON object.", what)));
    };
    Ok(map
        .iter()
        .filter(|(k, _)| !is_reserved_field(k))
        .map(|(k, v)| (k.clone(), if v.is_null() { None } else { Some(FieldValue::from_json(v)) }))
        .collect())
}

/// Apply a merge-patch. Returns true if anything changed.
fn apply_changes(data: &mut CompactFields, changes: &Changes) -> bool {
    let mut changed = false;
    for (k, v) in changes {
        match v {
            Some(v) => {
                if data.get(k) != Some(v) {
                    data.insert(k.clone(), v.clone());
                    changed = true;
                }
            }
            None => changed |= data.remove(k).is_some(),
        }
    }
    changed
}

fn check_bulk_size(len: usize) -> Result<()> {
    if len == 0 {
        return Err(invalid("The request contains no documents."));
    }
    if len > MAX_BULK_ITEMS {
        return Err(invalid(format!(
            "At most {} documents can be written in one request; got {}.",
            MAX_BULK_ITEMS, len
        )));
    }
    Ok(())
}

fn not_found_ids(tess: &str, missing: &[String]) -> anyhow::Error {
    let shown: Vec<&str> = missing.iter().take(10).map(String::as_str).collect();
    let more = if missing.len() > shown.len() { format!(" and {} more", missing.len() - shown.len()) } else { String::new() };
    EngineError::NotFound(format!(
        "{} document(s) not found in '{}': {}{}.",
        missing.len(),
        tess,
        shown.join(", "),
        more
    ))
    .into()
}

fn validate_tess(tess: &str) -> Result<()> {
    validate_tessellation_name(tess).map_err(|e| invalid(e.to_string()))
}

impl HexDBEngine {
    // -----------------------------------------------------------------------
    // Reads
    // -----------------------------------------------------------------------

    /// Visible documents in a tessellation in ID order, starting after `after`.
    pub async fn list_documents(&self, tess: &str, after: Option<Ulid>, limit: usize) -> Result<ListPage> {
        // One extra ID tells whether there's a next page.
        let ids = self.visible_ids_after(tess, after, limit.saturating_add(1)).await;

        let mut documents = Vec::new();
        let mut next = None;
        for id in ids {
            if documents.len() == limit {
                next = documents.last().map(|d: &Document| d.id);
                break;
            }
            if let Some(doc) = self.read_latest(&DocKey::new(tess, id)).await?.0 {
                documents.push(doc);
            }
        }
        Ok(ListPage { documents, next })
    }

    /// Documents matching a filter, optionally sorted, one page at a time.
    /// Without `sort`, results are in ID order and `next` can be passed as
    /// `after` for the following page; with `sort`, page with `offset`.
    pub async fn query_documents(&self, tess: &str, query: &DocumentQuery) -> Result<QueryPage> {
        self.queries_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if query.after.is_some() && !query.sort.is_empty() {
            return Err(invalid("after can't be combined with sort; use offset to page sorted results."));
        }
        // No filter and no sort: read just the page, in ID order. The total
        // comes from the cached count.
        if query.filter.is_empty() && query.sort.is_empty() {
            let want = query.offset.saturating_add(query.limit).saturating_add(1);
            let ids = self.visible_ids_after(tess, query.after, want).await;
            let scanned = ids.len();
            let mut documents = Vec::new();
            let mut more = false;
            for id in ids.into_iter().skip(query.offset) {
                if documents.len() == query.limit {
                    more = true;
                    break;
                }
                if let Some(doc) = self.read_latest(&DocKey::new(tess, id)).await?.0 {
                    documents.push(doc);
                }
            }
            let next = if more { documents.last().map(|d| d.id) } else { None };
            let total = self.count_documents(tess).await?;
            return Ok(QueryPage { documents, total, next, indexes: Vec::new(), scanned });
        }

        // No filter, one sort key with a field index: walk the index in order.
        if query.filter.is_empty() && query.sort.len() == 1 && query.after.is_none() {
            if let Some(page) = self.query_sorted_by_index(tess, query).await? {
                return Ok(page);
            }
        }

        let (filter, plan) = self.prepare_filter(tess, &query.filter);
        let indexes = plan.as_ref().map(|p| p.indexes.clone()).unwrap_or_default();
        // Every candidate is checked so `total` covers all pages; `after`
        // only decides where the returned page starts.
        let ids = self.candidate_ids(tess, plan, None).await;
        let scanned = ids.len();

        let mut matched = Vec::new();
        for id in ids {
            if let Some(doc) = self.read_latest(&DocKey::new(tess, id)).await?.0 {
                if filter.matches(&doc) {
                    matched.push(doc);
                }
            }
        }
        let total = matched.len();
        sort_documents(&mut matched, &query.sort);

        let start = match query.after {
            Some(after) => matched.partition_point(|d| d.id <= after),
            None => 0,
        };
        let remaining = matched.len() - start;
        let documents: Vec<Document> = matched.into_iter().skip(start + query.offset).take(query.limit).collect();
        let next = if query.sort.is_empty() && query.offset + documents.len() < remaining {
            documents.last().map(|d| d.id)
        } else {
            None
        };
        Ok(QueryPage { documents, total, next, indexes, scanned })
    }

    /// IDs to read for a query, in ID order: the planner's candidates, or
    /// every visible document.
    async fn candidate_ids(&self, tess: &str, plan: Option<crate::index::Plan>, after: Option<Ulid>) -> Vec<Ulid> {
        match plan {
            Some(plan) => plan.candidates.into_iter().filter(|id| after.is_none_or(|a| *id > a)).collect(),
            None => {
                let mut ids: Vec<Ulid> = self
                    .versions(tess)
                    .await
                    .into_iter()
                    .filter(|(id, v)| v.visible() && after.is_none_or(|a| *id > a))
                    .map(|(id, _)| id)
                    .collect();
                ids.sort();
                ids
            }
        }
    }

    /// Every visible document matching a filter, in ID order (using indexes).
    pub async fn matching_documents(&self, tess: &str, filter: &Filter) -> Result<Vec<Document>> {
        let (filter, plan) = self.prepare_filter(tess, filter);
        let ids = self.candidate_ids(tess, plan, None).await;
        let mut matched = Vec::new();
        for id in ids {
            if let Some(doc) = self.read_latest(&DocKey::new(tess, id)).await?.0 {
                if filter.matches(&doc) {
                    matched.push(doc);
                }
            }
        }
        Ok(matched)
    }

    /// Like `matching_documents`, but always reads every document (no indexes).
    pub(crate) async fn matching_documents_scan(&self, tess: &str, filter: &Filter) -> Result<Vec<Document>> {
        let ids = self.candidate_ids(tess, None, None).await;
        let mut matched = Vec::new();
        for id in ids {
            if let Some(doc) = self.read_latest(&DocKey::new(tess, id)).await?.0 {
                if filter.matches(&doc) {
                    matched.push(doc);
                }
            }
        }
        Ok(matched)
    }

    /// Group and summarize the documents that match the aggregation's filter.
    pub async fn aggregate(&self, tess: &str, aggregation: &crate::aggregate::Aggregation) -> Result<crate::aggregate::AggregateResult> {
        self.queries_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let documents = self.matching_documents(tess, &aggregation.filter).await?;
        aggregation.run(&documents)
    }

    /// Number of documents matching a filter.
    pub async fn count_matching(&self, tess: &str, filter: &Filter) -> Result<usize> {
        if filter.is_empty() {
            return self.count_documents(tess).await;
        }
        let query = DocumentQuery { filter: filter.clone(), limit: 0, ..DocumentQuery::default() };
        Ok(self.query_documents(tess, &query).await?.total)
    }

    // -----------------------------------------------------------------------
    // Writes
    // -----------------------------------------------------------------------

    /// Insert one JSON object as a new document. `ttl` is an expiry time in epoch milliseconds.
    pub async fn insert_json(&self, tess: &str, json: Value, ttl: Option<i64>) -> Result<Document> {
        let mut docs = self.insert_documents(tess, vec![json], ttl, None).await?.value;
        Ok(docs.remove(0))
    }

    /// Insert documents atomically: all are stored or none are. IDs are
    /// assigned by the server; `id` and `_expires_at` fields in the input are ignored.
    pub async fn insert_documents(
        &self,
        tess: &str,
        docs: Vec<Value>,
        ttl: Option<i64>,
        idem: Option<IdempotencyKey>,
    ) -> Result<Outcome<Vec<Document>>> {
        validate_tess(tess)?;
        check_bulk_size(docs.len())?;
        let fields: Vec<CompactFields> = docs
            .iter()
            .enumerate()
            .map(|(i, json)| object_fields(json, &format!("documents[{}]", i)))
            .collect::<Result<_>>()?;
        self.ensure_tessellation(tess)?;

        let fields = &fields;
        self.run_write(idem, move || Box::pin(self.plan_insert(tess, fields, ttl))).await
    }

    /// Replace the fields of one document. The TTL is kept unless a new one is given.
    pub async fn replace_document(
        &self,
        tess: &str,
        id: &str,
        json: Value,
        ttl: Option<i64>,
        idem: Option<IdempotencyKey>,
    ) -> Result<Outcome<Document>> {
        let outcome = self.replace_documents(tess, vec![(id.to_string(), json)], ttl, idem).await?;
        Ok(outcome.map(|mut docs| docs.remove(0)))
    }

    /// Merge fields into one document (a `null` field removes it).
    pub async fn patch_document(
        &self,
        tess: &str,
        id: &str,
        json: Value,
        ttl: Option<i64>,
        idem: Option<IdempotencyKey>,
    ) -> Result<Outcome<Document>> {
        let outcome = self.patch_documents(tess, vec![(id.to_string(), json)], ttl, idem).await?;
        Ok(outcome.map(|mut docs| docs.remove(0)))
    }

    /// Replace several documents atomically. Fails with NotFound, writing nothing,
    /// if any of them doesn't exist.
    pub async fn replace_documents(
        &self,
        tess: &str,
        items: Vec<(String, Value)>,
        ttl: Option<i64>,
        idem: Option<IdempotencyKey>,
    ) -> Result<Outcome<Vec<Document>>> {
        let targets = self.parse_targets(tess, items, |json, what| Ok(Modification::Replace(object_fields(json, what)?)))?;
        let targets = &targets;
        self.run_write(idem, move || Box::pin(self.plan_modify(tess, targets, ttl))).await
    }

    /// Merge-patch several documents atomically (a `null` field removes it).
    /// Fails with NotFound, writing nothing, if any of them doesn't exist.
    pub async fn patch_documents(
        &self,
        tess: &str,
        items: Vec<(String, Value)>,
        ttl: Option<i64>,
        idem: Option<IdempotencyKey>,
    ) -> Result<Outcome<Vec<Document>>> {
        let targets = self.parse_targets(tess, items, |json, what| Ok(Modification::Patch(patch_changes(json, what)?)))?;
        let targets = &targets;
        self.run_write(idem, move || Box::pin(self.plan_modify(tess, targets, ttl))).await
    }

    /// Delete a document. The value is false if it didn't exist.
    pub async fn delete_document(&self, tess: &str, id: &str, idem: Option<IdempotencyKey>) -> Result<Outcome<bool>> {
        validate_tess(tess)?;
        let Ok(id) = Ulid::from_string(id) else {
            return Ok(Outcome { value: false, replayed: false });
        };
        self.run_write(idem, move || Box::pin(self.plan_delete(tess, id))).await
    }

    /// Merge-patch every document matching `filter` (see [`crate::filter`]).
    /// An empty filter matches every document. Atomic.
    pub async fn update_where(
        &self,
        tess: &str,
        filter: &Value,
        update: &Value,
        ttl: Option<i64>,
        idem: Option<IdempotencyKey>,
    ) -> Result<Outcome<UpdateSummary>> {
        validate_tess(tess)?;
        let filter = Filter::parse(filter)?;
        let changes = patch_changes(update, "update")?;
        if changes.is_empty() && ttl.is_none() {
            return Err(invalid("update: nothing to change."));
        }

        let (filter, changes) = (&filter, &changes);
        self.run_write(idem, move || Box::pin(self.plan_update_where(tess, filter, changes, ttl))).await
    }

    // -----------------------------------------------------------------------
    // Planning
    // -----------------------------------------------------------------------

    fn parse_targets(
        &self,
        tess: &str,
        items: Vec<(String, Value)>,
        parse: impl Fn(&Value, &str) -> Result<Modification>,
    ) -> Result<Vec<(Ulid, String, Modification)>> {
        validate_tess(tess)?;
        check_bulk_size(items.len())?;
        let mut seen = HashSet::new();
        let mut targets = Vec::with_capacity(items.len());
        for (i, (id_str, json)) in items.into_iter().enumerate() {
            let what = format!("documents[{}]", i);
            let id = Ulid::from_string(&id_str).unwrap_or(Ulid::nil());
            if !seen.insert(id_str.clone()) {
                return Err(invalid(format!("{}: document {} appears more than once.", what, id_str)));
            }
            targets.push((id, id_str, parse(&json, &what)?));
        }
        Ok(targets)
    }

    async fn plan_insert(&self, tess: &str, fields: &[CompactFields], ttl: Option<i64>) -> Result<(Vec<BatchItem>, Vec<Document>)> {
        let docs: Vec<Document> = fields
            .iter()
            .map(|data| Document { id: Ulid::new(), tessellation: tess.to_string(), data: data.clone(), ttl })
            .collect();
        let items = docs
            .iter()
            .map(|doc| BatchItem { key: DocKey::new(tess, doc.id), op: WalOp::Put(doc.clone()), expected_seq: None })
            .collect();
        Ok((items, docs))
    }

    async fn plan_modify(
        &self,
        tess: &str,
        targets: &[(Ulid, String, Modification)],
        ttl: Option<i64>,
    ) -> Result<(Vec<BatchItem>, Vec<Document>)> {
        let mut items = Vec::with_capacity(targets.len());
        let mut docs = Vec::with_capacity(targets.len());
        let mut missing = Vec::new();

        for (id, id_str, modification) in targets {
            let key = DocKey::new(tess, *id);
            let (current, seq) = if id.is_nil() { (None, 0) } else { self.read_latest(&key).await? };
            let Some(mut doc) = current else {
                missing.push(id_str.clone());
                continue;
            };
            match modification {
                Modification::Replace(fields) => doc.data = fields.clone(),
                Modification::Patch(changes) => {
                    apply_changes(&mut doc.data, changes);
                }
            }
            if ttl.is_some() {
                doc.ttl = ttl;
            }
            items.push(BatchItem { key, op: WalOp::Put(doc.clone()), expected_seq: Some(seq) });
            docs.push(doc);
        }

        if !missing.is_empty() {
            return Err(not_found_ids(tess, &missing));
        }
        Ok((items, docs))
    }

    async fn plan_delete(&self, tess: &str, id: Ulid) -> Result<(Vec<BatchItem>, bool)> {
        let key = DocKey::new(tess, id);
        let (current, seq) = self.read_latest(&key).await?;
        if current.is_none() {
            return Ok((Vec::new(), false));
        }
        let op = WalOp::Delete { tessellation: tess.to_string(), id };
        Ok((vec![BatchItem { key, op, expected_seq: Some(seq) }], true))
    }

    async fn plan_update_where(
        &self,
        tess: &str,
        filter: &Filter,
        changes: &Changes,
        ttl: Option<i64>,
    ) -> Result<(Vec<BatchItem>, UpdateSummary)> {
        let (filter, plan) = self.prepare_filter(tess, filter);
        let filter = &filter;
        let ids = self.candidate_ids(tess, plan, None).await;

        let mut summary = UpdateSummary::default();
        let mut items = Vec::new();
        for id in ids {
            let key = DocKey::new(tess, id);
            let (Some(mut doc), seq) = self.read_latest(&key).await? else { continue };

            if !filter.matches(&doc) {
                continue;
            }
            summary.matched += 1;

            let mut changed = apply_changes(&mut doc.data, changes);
            if ttl.is_some() && doc.ttl != ttl {
                doc.ttl = ttl;
                changed = true;
            }
            if changed {
                summary.modified += 1;
                if summary.modified > MAX_UPDATE_MATCHES {
                    return Err(invalid(format!(
                        "The update would modify more than {} documents; narrow the filter.",
                        MAX_UPDATE_MATCHES
                    )));
                }
                items.push(BatchItem { key, op: WalOp::Put(doc), expected_seq: Some(seq) });
            }
        }
        Ok((items, summary))
    }

    // -----------------------------------------------------------------------
    // Idempotency and commit
    // -----------------------------------------------------------------------

    /// Plan and commit a write, retrying on concurrent modification, with
    /// optional idempotency.
    async fn run_write<'a, T>(
        &'a self,
        idem: Option<IdempotencyKey>,
        plan: impl Fn() -> BoxFuture<'a, Result<(Vec<BatchItem>, T)>>,
    ) -> Result<Outcome<T>>
    where
        T: Serialize + DeserializeOwned,
    {
        let _guard = match &idem {
            Some(k) => {
                self.ensure_system_tessellation(IDEMPOTENCY_TESSELLATION)?;
                Some(self.acquire_inflight(&k.key)?)
            }
            None => None,
        };

        for _ in 0..MAX_WRITE_RETRIES {
            let mut idem_seq = 0;
            if let Some(k) = &idem {
                match self.idempotency_lookup(k).await? {
                    IdempotencyLookup::Replay(result, _) => {
                        let value = serde_json::from_value(result)
                            .map_err(|e| anyhow!("Stored idempotent result is unreadable: {}", e))?;
                        return Ok(Outcome { value, replayed: true });
                    }
                    IdempotencyLookup::Absent { seq } => idem_seq = seq,
                }
            }

            let (mut items, value) = plan().await?;
            if let Some(k) = &idem {
                items.push(self.idempotency_item(k, &value, idem_seq)?);
            }
            if self.commit(items).await? {
                return Ok(Outcome { value, replayed: false });
            }
        }
        Err(EngineError::Conflict("The documents are being modified concurrently; try again.".into()).into())
    }

    /// The stored result for an idempotency key, if this exact request already
    /// succeeded. Errors if the key was used with a different request.
    pub(crate) async fn replayed<T: DeserializeOwned>(&self, k: &IdempotencyKey) -> Result<Option<T>> {
        match self.idempotency_lookup(k).await? {
            IdempotencyLookup::Replay(result, _) => Ok(Some(
                serde_json::from_value(result).map_err(|e| anyhow!("Stored idempotent result is unreadable: {}", e))?,
            )),
            IdempotencyLookup::Absent { .. } => Ok(None),
        }
    }

    pub(crate) fn acquire_inflight(&self, key: &str) -> Result<InflightGuard<'_>> {
        if !self.inflight.lock().unwrap().insert(key.to_string()) {
            return Err(EngineError::Conflict(
                "A request with this Idempotency-Key is already in progress.".into(),
            )
            .into());
        }
        Ok(InflightGuard { engine: self, key: key.to_string() })
    }

    /// The stored result for an idempotency key (if this request already
    /// succeeded) and the record's version, for `expected_seq`.
    pub(crate) async fn idempotency_lookup_seq(&self, k: &IdempotencyKey) -> Result<(Option<Value>, u64)> {
        Ok(match self.idempotency_lookup(k).await? {
            IdempotencyLookup::Replay(result, seq) => (Some(result), seq),
            IdempotencyLookup::Absent { seq } => (None, seq),
        })
    }

    async fn idempotency_lookup(&self, k: &IdempotencyKey) -> Result<IdempotencyLookup> {
        let key = DocKey::new(IDEMPOTENCY_TESSELLATION, k.doc_id());
        let (doc, seq) = self.read_latest(&key).await?;
        let Some(doc) = doc else { return Ok(IdempotencyLookup::Absent { seq }) };

        let data = doc.data_json();
        if data.get("key").and_then(Value::as_str) != Some(k.key.as_str()) {
            // A different key with the same hash (vanishingly unlikely); treat as unused.
            return Ok(IdempotencyLookup::Absent { seq });
        }
        if data.get("fingerprint").and_then(Value::as_str) != Some(k.fingerprint.as_str()) {
            return Err(EngineError::Unprocessable(
                "This Idempotency-Key was already used with a different request.".into(),
            )
            .into());
        }
        Ok(IdempotencyLookup::Replay(data.get("result").cloned().unwrap_or(Value::Null), seq))
    }

    pub(crate) fn idempotency_item(&self, k: &IdempotencyKey, result: &impl Serialize, expected_seq: u64) -> Result<BatchItem> {
        let now = Utc::now().timestamp_millis();
        let mut data = CompactFields::new();
        data.insert("key".into(), FieldValue::String(k.key.clone()));
        data.insert("fingerprint".into(), FieldValue::String(k.fingerprint.clone()));
        let mut result = serde_json::to_value(result)?;
        if k.compact {
            // Documents keep their ID and metadata; their fields are dropped.
            if let Value::Array(items) = &mut result {
                for item in items {
                    if let Some(data) = item.get_mut("data") {
                        *data = Value::Object(Default::default());
                    }
                }
            }
        }
        data.insert("result".into(), FieldValue::from_json(&result));
        data.insert("created".into(), FieldValue::Integer(now));

        let doc = Document {
            id: k.doc_id(),
            tessellation: IDEMPOTENCY_TESSELLATION.into(),
            data,
            ttl: Some(now + IDEMPOTENCY_TTL_MILLIS),
        };
        Ok(BatchItem {
            key: DocKey::new(IDEMPOTENCY_TESSELLATION, doc.id),
            op: WalOp::Put(doc),
            expected_seq: Some(expected_seq),
        })
    }

    /// Apply operations atomically. Returns false, changing nothing, if any
    /// expected version no longer matches. Returns once the batch is durable.
    pub(crate) async fn commit(&self, items: Vec<BatchItem>) -> Result<bool> {
        Ok(self.commit_checked(items, &[]).await?.is_some())
    }

    /// Like `commit`, and also verify that `checks` (documents that were read
    /// but not written) still have the given versions. Returns the sequence
    /// number of the first item written (for a read-only batch, the next
    /// sequence number), or `None`, changing nothing, if any version changed.
    pub(crate) async fn commit_checked(&self, items: Vec<BatchItem>, checks: &[(DocKey, u64)]) -> Result<Option<u64>> {
        self.commit_with(items, checks, None).await
    }

    /// `commit_checked`, plus an optional trailing item built under the commit
    /// lock from the batch's first sequence number. Transactions use it to
    /// store an idempotency record that includes the versions being assigned.
    pub(crate) async fn commit_with(
        &self,
        items: Vec<BatchItem>,
        checks: &[(DocKey, u64)],
        trailer: Option<&(dyn Fn(u64) -> Result<BatchItem> + Sync)>,
    ) -> Result<Option<u64>> {
        self.commit_inner(items, checks, trailer, false).await
    }

    /// Apply writes received from the Overseer: no write guard and no unique
    /// checks (the Overseer already enforced them, and a batch can pass
    /// through states that only look like duplicates).
    pub(crate) async fn commit_replicated(&self, items: Vec<BatchItem>) -> Result<()> {
        self.commit_inner(items, &[], None, true).await.map(|_| ())
    }

    async fn commit_inner(
        &self,
        items: Vec<BatchItem>,
        checks: &[(DocKey, u64)],
        trailer: Option<&(dyn Fn(u64) -> Result<BatchItem> + Sync)>,
        replicated: bool,
    ) -> Result<Option<u64>> {
        if !replicated && (!items.is_empty() || trailer.is_some()) {
            self.ensure_writable()?;
        }
        if items.is_empty() && trailer.is_none() {
            let state = self.state.lock().await;
            for (key, expected) in checks {
                if self.current_seq(&state, key).await != *expected {
                    return Ok(None);
                }
            }
            return Ok(Some(state.next_seq));
        }
        if self.wal.has_failed() {
            bail!("Writes are disabled after a WAL failure. Check the disk and restart HexDB.");
        }
        {
            let mut seen = HashSet::new();
            if !items.iter().all(|i| seen.insert(&i.key)) {
                return Err(invalid("The same document appears more than once in one request."));
            }
        }

        let user_writes = items.iter().filter(|i| i.key.tessellation != IDEMPOTENCY_TESSELLATION).count() as u64;

        fn prepare(item: &BatchItem) -> Result<(DocKey, Option<Vec<u8>>, Option<i64>)> {
            match &item.op {
                WalOp::Put(doc) => Ok((item.key.clone(), Some(serde_json::to_vec(doc)?), doc.ttl)),
                WalOp::Delete { .. } => Ok((item.key.clone(), None, None)),
                WalOp::Batch(_) => Err(anyhow!("Nested batches are not supported")),
            }
        }
        let mut prepared: Vec<(DocKey, Option<Vec<u8>>, Option<i64>)> = items.iter().map(prepare).collect::<Result<_>>()?;
        let mut items = items;

        let (ack, first, changes) = {
            let mut state = self.state.lock().await;
            for item in &items {
                if let Some(expected) = item.expected_seq {
                    if self.current_seq(&state, &item.key).await != expected {
                        return Ok(None);
                    }
                }
            }
            for (key, expected) in checks {
                if self.current_seq(&state, key).await != *expected {
                    return Ok(None);
                }
            }

            let first = state.next_seq;
            if let Some(build) = trailer {
                let item = build(first)?;
                if let Some(expected) = item.expected_seq {
                    if self.current_seq(&state, &item.key).await != expected {
                        return Ok(None);
                    }
                }
                prepared.push(prepare(&item)?);
                items.push(item);
            }
            if !replicated {
                self.check_unique(&items)?;
            }
            let count = items.len() as u64;
            let now = Utc::now();
            let changes: Vec<Change> = items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    let (kind, document) = match &item.op {
                        WalOp::Put(doc) => (ChangeKind::Put, Some(doc.clone())),
                        WalOp::Delete { .. } => (ChangeKind::Delete, None),
                        WalOp::Batch(_) => return None,
                    };
                    Some(Change {
                        seq: first + i as u64,
                        timestamp: now,
                        kind,
                        tessellation: item.key.tessellation.clone(),
                        id: Some(item.key.id),
                        document,
                    })
                })
                .collect();
            let mut ops: Vec<WalOp> = items.into_iter().map(|i| i.op).collect();
            let op = if ops.len() == 1 { ops.pop().unwrap() } else { WalOp::Batch(ops) };

            // Queue to the WAL first so nothing is applied if the WAL is unavailable.
            let ack = self.wal.append(WalRecord { seq: first, op }).await?;
            state.next_seq += count;
            let changes = (changes, count);
            for (i, (key, bytes, ttl)) in prepared.iter().enumerate() {
                let seq = first + i as u64;
                match bytes {
                    Some(bytes) => state.hex.put(key, seq, *ttl, bytes, true),
                    None => state.hex.put_tombstone(key, seq, true),
                }
            }
            self.bump_generation();
            {
                let mut indexes = self.indexes.write().unwrap();
                for change in &changes.0 {
                    if let Some(id) = change.id {
                        indexes.apply(&change.tessellation, &id, change.document.as_ref());
                    }
                }
            }
            if state.hex.dirty_bytes() >= self.flush_threshold {
                self.flush_needed.notify_one();
            }
            (ack, first, changes)
        };
        let (ack, first, (changes, count)) = (ack, first, changes);

        // Publish once durable. A failed write still consumes its sequence
        // numbers, so the feed is told about the range either way.
        if let Err(e) = ack.wait().await {
            self.changes.complete(first, count, Vec::new());
            return Err(e);
        }
        self.changes.complete(first, count, changes);
        self.writes_total.fetch_add(user_writes, std::sync::atomic::Ordering::Relaxed);
        Ok(Some(first))
    }

    /// Fail with a conflict if a write would break a unique index.
    fn check_unique(&self, items: &[BatchItem]) -> Result<()> {
        let indexes = self.indexes.read().unwrap();
        let mut changing: std::collections::HashMap<&str, HashSet<Ulid>> = std::collections::HashMap::new();
        for item in items {
            changing.entry(item.key.tessellation.as_str()).or_default().insert(item.key.id);
        }
        let mut claimed: HashSet<(String, String, Vec<String>)> = HashSet::new();
        for item in items {
            let WalOp::Put(doc) = &item.op else { continue };
            let Some(list) = indexes.by_tessellation.get(&doc.tessellation) else { continue };
            for index in list.iter().filter(|i| i.def.unique) {
                if let Some(other) = index.unique_conflict(doc, &changing[doc.tessellation.as_str()]) {
                    return Err(EngineError::Conflict(format!(
                        "Unique index '{}' on '{}': document {} already has this {}.",
                        index.def.name,
                        doc.tessellation,
                        other,
                        index.def.fields.join(" + ")
                    ))
                    .into());
                }
                for key in index.unique_keys(doc) {
                    if !claimed.insert((doc.tessellation.clone(), index.def.name.clone(), key)) {
                        return Err(EngineError::Conflict(format!(
                            "Unique index '{}' on '{}': two documents in this request share a {}.",
                            index.def.name,
                            doc.tessellation,
                            index.def.fields.join(" + ")
                        ))
                        .into());
                    }
                }
            }
        }
        Ok(())
    }

    /// Sequence number of a document's newest version (0 if it never existed).
    async fn current_seq(&self, state: &super::EngineState, key: &DocKey) -> u64 {
        match state.hex.meta(key) {
            Some(meta) => meta.seq,
            None => self.sst.seq_of(&key.tessellation, &key.id).await.unwrap_or(0),
        }
    }
}
