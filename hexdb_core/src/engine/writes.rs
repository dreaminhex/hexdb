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
        Ok(IdempotencyKey { key: key.to_string(), fingerprint: blake3::hash(request).to_hex().to_string() })
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
    Replay(Value),
}

struct InflightGuard<'a> {
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
        let mut ids: Vec<Ulid> = self
            .versions(tess)
            .await
            .into_iter()
            .filter(|(id, v)| v.visible() && after.is_none_or(|a| *id > a))
            .map(|(id, _)| id)
            .collect();
        ids.sort();

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
        let mut ids: Vec<Ulid> = self
            .versions(tess)
            .await
            .into_iter()
            .filter(|(id, v)| v.visible() && query.after.is_none_or(|a| *id > a))
            .map(|(id, _)| id)
            .collect();
        ids.sort();

        let mut matched = Vec::new();
        for id in ids {
            if let Some(doc) = self.read_latest(&DocKey::new(tess, id)).await?.0 {
                if query.filter.matches(&doc) {
                    matched.push(doc);
                }
            }
        }
        let total = matched.len();
        sort_documents(&mut matched, &query.sort);

        let documents: Vec<Document> = matched.into_iter().skip(query.offset).take(query.limit).collect();
        let next = if query.sort.is_empty() && query.offset + documents.len() < total {
            documents.last().map(|d| d.id)
        } else {
            None
        };
        Ok(QueryPage { documents, total, next })
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
        let mut ids: Vec<Ulid> = self
            .versions(tess)
            .await
            .into_iter()
            .filter(|(_, v)| v.visible())
            .map(|(id, _)| id)
            .collect();
        ids.sort();

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
                    IdempotencyLookup::Replay(result) => {
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
            IdempotencyLookup::Replay(result) => Ok(Some(
                serde_json::from_value(result).map_err(|e| anyhow!("Stored idempotent result is unreadable: {}", e))?,
            )),
            IdempotencyLookup::Absent { .. } => Ok(None),
        }
    }

    fn acquire_inflight(&self, key: &str) -> Result<InflightGuard<'_>> {
        if !self.inflight.lock().unwrap().insert(key.to_string()) {
            return Err(EngineError::Conflict(
                "A request with this Idempotency-Key is already in progress.".into(),
            )
            .into());
        }
        Ok(InflightGuard { engine: self, key: key.to_string() })
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
        Ok(IdempotencyLookup::Replay(data.get("result").cloned().unwrap_or(Value::Null)))
    }

    fn idempotency_item(&self, k: &IdempotencyKey, result: &impl Serialize, expected_seq: u64) -> Result<BatchItem> {
        let now = Utc::now().timestamp_millis();
        let mut data = CompactFields::new();
        data.insert("key".into(), FieldValue::String(k.key.clone()));
        data.insert("fingerprint".into(), FieldValue::String(k.fingerprint.clone()));
        data.insert("result".into(), FieldValue::from_json(&serde_json::to_value(result)?));
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
        if items.is_empty() {
            return Ok(true);
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

        let prepared: Vec<(DocKey, Option<Vec<u8>>, Option<i64>)> = items
            .iter()
            .map(|item| match &item.op {
                WalOp::Put(doc) => Ok((item.key.clone(), Some(serde_json::to_vec(doc)?), doc.ttl)),
                WalOp::Delete { .. } => Ok((item.key.clone(), None, None)),
                WalOp::Batch(_) => Err(anyhow!("Nested batches are not supported")),
            })
            .collect::<Result<_>>()?;

        let ack = {
            let mut state = self.state.lock().await;
            for item in &items {
                if let Some(expected) = item.expected_seq {
                    let current = match state.hex.meta(&item.key) {
                        Some(meta) => meta.seq,
                        None => self.sst.seq_of(&item.key.tessellation, &item.key.id).await.unwrap_or(0),
                    };
                    if current != expected {
                        return Ok(false);
                    }
                }
            }

            let first = state.next_seq;
            let count = items.len() as u64;
            let mut ops: Vec<WalOp> = items.into_iter().map(|i| i.op).collect();
            let op = if ops.len() == 1 { ops.pop().unwrap() } else { WalOp::Batch(ops) };

            // Queue to the WAL first so nothing is applied if the WAL is unavailable.
            let ack = self.wal.append(WalRecord { seq: first, op }).await?;
            state.next_seq += count;
            for (i, (key, bytes, ttl)) in prepared.iter().enumerate() {
                let seq = first + i as u64;
                match bytes {
                    Some(bytes) => state.hex.put(key, seq, *ttl, bytes, true),
                    None => state.hex.put_tombstone(key, seq, true),
                }
            }
            if state.hex.dirty_bytes() >= self.flush_threshold {
                self.flush_needed.notify_one();
            }
            ack
        };

        ack.wait().await?;
        self.writes_total.fetch_add(user_writes, std::sync::atomic::Ordering::Relaxed);
        Ok(true)
    }
}
