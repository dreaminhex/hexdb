// HexDB Core Engine: transactions
//
// A transaction is a list of operations across any user tessellations that
// commits atomically: every operation succeeds and is written in one WAL
// record, or nothing is written ("rollback"). Operations see the effects of
// earlier operations in the same transaction.
//
//   {
//     "operations": [
//       { "op": "get",     "tessellation": "accounts", "id": "01J..." },
//       { "op": "patch",   "tessellation": "accounts", "id": "01J...", "data": { "balance": 70 },
//         "if_match": { "balance": { "$gte": 30 } } },
//       { "op": "insert",  "tessellation": "ledger", "data": { "amount": -30 } },
//       { "op": "replace", "tessellation": "x", "id": "...", "data": { ... }, "if_version": 42 },
//       { "op": "delete",  "tessellation": "x", "id": "..." },
//       { "op": "check",   "tessellation": "x", "id": "...", "if_version": 0 }
//     ]
//   }
//
// Isolation: every document the transaction reads or writes is validated at
// commit. If another write changed any of them in the meantime, the
// transaction is planned again from the new state (and its preconditions
// re-checked), so the result is as if it ran alone (serializable for the
// documents it touches).
//
// Preconditions, on any operation that names an `id`:
//   if_version: the document's version (sequence number) must equal this;
//               0 means the document must not exist.
//   if_match:   the document must exist and match this filter.
// A failed precondition aborts the whole transaction with a conflict.
//
// Versions: each result carries the document's version after the commit.
// Pass it as `if_version` later for optimistic concurrency.

use super::{writes::BatchItem, EngineError, HexDBEngine, MAX_WRITE_RETRIES};
use crate::auth::Action;
use crate::{
    catalog::validate_tessellation_name,
    document::{is_reserved_field, CompactFields, Document, FieldValue},
    filter::Filter,
    hex::DocKey,
    wal::WalOp,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use ulid::Ulid;

use super::writes::{IdempotencyKey, Outcome};

/// Most operations in one transaction.
pub const MAX_TRANSACTION_OPS: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TxOpKind {
    /// Read a document (validated at commit).
    Get,
    /// Assert preconditions on a document without returning it.
    Check,
    Insert,
    Replace,
    Patch,
    Delete,
}

/// One operation in a transaction.
#[derive(Debug, Clone)]
pub struct TxOperation {
    pub kind: TxOpKind,
    pub tessellation: String,
    /// Required except for `insert`, where it optionally picks the new document's ID.
    pub id: Option<Ulid>,
    /// Document body (insert/replace) or merge-patch (patch; null removes a field).
    pub data: Option<Value>,
    /// Expiry in epoch milliseconds (insert/replace/patch). Requests give
    /// `ttl` in seconds from now, like the rest of the API.
    pub ttl: Option<i64>,
    pub if_version: Option<u64>,
    pub if_match: Option<Filter>,
}

/// The outcome of one operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TxResult {
    pub op: TxOpKind,
    pub tessellation: String,
    pub id: String,
    /// The document's version after the commit (`None` once deleted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    /// The document as of this operation (get/insert/replace/patch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<Value>,
}

/// A committed transaction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransactionResult {
    pub results: Vec<TxResult>,
    /// Documents written.
    pub writes: usize,
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    EngineError::Invalid(message.into()).into()
}

fn conflict(message: impl Into<String>) -> anyhow::Error {
    EngineError::Conflict(message.into()).into()
}

impl TxOperation {
    /// Parse one operation object.
    pub fn from_json(value: &Value, index: usize) -> Result<TxOperation> {
        let at = |m: &str| invalid(format!("operations[{}]: {}", index, m));
        let Value::Object(map) = value else { return Err(at("expected an object.")) };
        for key in map.keys() {
            if !["op", "tessellation", "id", "data", "ttl", "if_version", "if_match"].contains(&key.as_str()) {
                return Err(at(&format!("unknown key '{}'.", key)));
            }
        }
        let kind: TxOpKind = map
            .get("op")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .ok_or_else(|| at("op must be one of get, check, insert, replace, patch, delete."))?;
        let tessellation = map
            .get("tessellation")
            .and_then(Value::as_str)
            .ok_or_else(|| at("tessellation is required."))?
            .to_string();
        if tessellation.starts_with('_') {
            return Err(at("system tessellations can't be used in transactions."));
        }
        validate_tessellation_name(&tessellation).map_err(|e| at(&e.to_string()))?;
        let id = match map.get("id") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(Ulid::from_string(s).map_err(|_| at(&format!("'{}' is not a valid document ID.", s)))?),
            Some(_) => return Err(at("id must be a string.")),
        };
        if id.is_none() && kind != TxOpKind::Insert {
            return Err(at("id is required."));
        }
        let data = map.get("data").filter(|v| !v.is_null()).cloned();
        match kind {
            TxOpKind::Insert | TxOpKind::Replace | TxOpKind::Patch => match &data {
                Some(Value::Object(_)) => {}
                _ => return Err(at("data must be an object.")),
            },
            _ if data.is_some() => return Err(at(&format!("{:?} takes no data.", kind).to_lowercase())),
            _ => {}
        }
        let ttl = match map.get("ttl") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let secs = v.as_u64().ok_or_else(|| at("ttl must be a number of seconds."))?;
                let millis = secs.min(i64::MAX as u64 / 1000) as i64 * 1000;
                Some(chrono::Utc::now().timestamp_millis().saturating_add(millis))
            }
        };
        if ttl.is_some() && matches!(kind, TxOpKind::Get | TxOpKind::Check | TxOpKind::Delete) {
            return Err(at("ttl only applies to insert, replace, and patch."));
        }
        let if_version = match map.get("if_version") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_u64().ok_or_else(|| at("if_version must be a non-negative integer."))?),
        };
        let if_match = match map.get("if_match") {
            None | Some(Value::Null) => None,
            Some(v) => Some(Filter::parse(v).map_err(|e| at(&e.to_string()))?),
        };
        if kind == TxOpKind::Insert && (if_match.is_some() || if_version.is_some_and(|v| v != 0)) {
            return Err(at("insert creates a new document; only if_version: 0 applies."));
        }
        Ok(TxOperation { kind, tessellation, id, data, ttl, if_version, if_match })
    }
}

/// Parse `{"operations": [...]}` (or a bare array of operations).
pub fn parse_transaction(body: &Value) -> Result<Vec<TxOperation>> {
    let ops = match body {
        Value::Array(items) => items,
        Value::Object(map) => {
            if let Some(key) = map.keys().find(|k| k.as_str() != "operations") {
                return Err(invalid(format!("unknown key '{}'.", key)));
            }
            match map.get("operations") {
                Some(Value::Array(items)) => items,
                _ => return Err(invalid("operations must be an array.")),
            }
        }
        _ => return Err(invalid("expected {\"operations\": [...]}.")),
    };
    if ops.is_empty() {
        return Err(invalid("The transaction has no operations."));
    }
    if ops.len() > MAX_TRANSACTION_OPS {
        return Err(invalid(format!("At most {} operations fit in one transaction; got {}.", MAX_TRANSACTION_OPS, ops.len())));
    }
    ops.iter().enumerate().map(|(i, v)| TxOperation::from_json(v, i)).collect()
}

fn fields(json: &Value) -> CompactFields {
    json.as_object()
        .map(|m| m.iter().filter(|(k, _)| !is_reserved_field(k)).map(|(k, v)| (k.clone(), FieldValue::from_json(v))).collect())
        .unwrap_or_default()
}

/// A document's state inside a transaction.
struct Slot {
    /// Version when first read (0 = never existed).
    base_seq: u64,
    current: Option<Document>,
    written: bool,
    /// The document exists but the caller's role can't see it.
    hidden: bool,
}

/// A planned transaction: the batch to commit plus the results (versions are
/// filled in after the commit).
struct Plan {
    items: Vec<BatchItem>,
    checks: Vec<(DocKey, u64)>,
    results: Vec<TxResult>,
    /// For each result, the index in `items` of its document's write, if written.
    write_index: Vec<Option<usize>>,
    /// For each result, the base version when the document wasn't written.
    read_version: Vec<Option<u64>>,
}

impl HexDBEngine {
    /// Run a transaction. Fails, writing nothing, if any operation fails.
    /// With `idem`, a retry of the same request returns the original result.
    pub async fn transaction(&self, ops: &[TxOperation], idem: Option<IdempotencyKey>) -> Result<Outcome<TransactionResult>> {
        for op in ops {
            if self.is_system_tessellation(&op.tessellation) {
                return Err(invalid(format!(
                    "'{}' is a system tessellation and can't be used in transactions.",
                    op.tessellation
                )));
            }
            if !self.tessellation_exists(&op.tessellation) && op.kind != TxOpKind::Insert {
                return Err(EngineError::NotFound(format!("Tessellation '{}' not found.", op.tessellation)).into());
            }
        }
        // Inserts create their tessellation, as single-document inserts do.
        for op in ops.iter().filter(|op| op.kind == TxOpKind::Insert) {
            self.ensure_tessellation(&op.tessellation)?;
        }
        let _guard = match &idem {
            Some(k) => {
                self.ensure_system_tessellation(super::writes::IDEMPOTENCY_TESSELLATION)?;
                Some(self.acquire_inflight(&k.key)?)
            }
            None => None,
        };

        for _ in 0..MAX_WRITE_RETRIES {
            let mut idem_seq = 0;
            if let Some(k) = &idem {
                match self.idempotency_lookup_seq(k).await? {
                    (Some(stored), _) => return Ok(Outcome { value: serde_json::from_value(stored)?, replayed: true }),
                    (None, seq) => idem_seq = seq,
                }
            }

            let mut plan = self.plan_transaction(ops).await?;
            let writes = plan.items.len();
            let items = std::mem::take(&mut plan.items);
            let finished = |first: u64| -> TransactionResult {
                let mut results = plan.results.clone();
                for (i, result) in results.iter_mut().enumerate() {
                    result.version = match (plan.write_index[i], plan.read_version[i]) {
                        (Some(index), _) => Some(first + index as u64),
                        (None, base) => base,
                    };
                }
                TransactionResult { results, writes }
            };

            let committed = match &idem {
                // The idempotency record is built under the commit lock, once
                // the versions this batch gets are known.
                Some(k) => {
                    let trailer = |first: u64| self.idempotency_item(k, &finished(first), idem_seq);
                    self.commit_with(items, &plan.checks, Some(&trailer)).await?
                }
                None => self.commit_checked(items, &plan.checks).await?,
            };
            if let Some(first) = committed {
                return Ok(Outcome { value: finished(first), replayed: false });
            }
        }
        Err(conflict("The documents in this transaction are being modified concurrently; try again."))
    }

    async fn plan_transaction(&self, ops: &[TxOperation]) -> Result<Plan> {
        let mut slots: HashMap<DocKey, Slot> = HashMap::new();
        // Keys in first-touched order, so the batch is deterministic.
        let mut order: Vec<DocKey> = Vec::new();
        let mut results = Vec::with_capacity(ops.len());
        let mut result_keys = Vec::with_capacity(ops.len());
        // The caller's row filters and field masks, per tessellation (read, write).
        let mut scopes: HashMap<String, (crate::access::Scope, crate::access::Scope)> = HashMap::new();

        for (i, op) in ops.iter().enumerate() {
            let at = |m: String| format!("operations[{}]: {}", i, m);
            let id = op.id.unwrap_or_else(Ulid::new);
            let key = DocKey::new(&op.tessellation, id);

            let (read, write) = scopes
                .entry(op.tessellation.clone())
                .or_insert_with(|| (self.caller_scope(&op.tessellation, Action::Read), self.caller_scope(&op.tessellation, Action::Write)))
                .clone();
            if let Some(filter) = &op.if_match {
                self.check_scope_fields(&op.tessellation, &read, filter, &[])?;
            }
            if !slots.contains_key(&key) {
                let (current, base_seq) = if op.id.is_some() { self.read_latest(&key).await? } else { (None, 0) };
                let hidden = current.as_ref().is_some_and(|d| !read.allows(d));
                let current = if hidden { None } else { current };
                slots.insert(key.clone(), Slot { base_seq, current, written: false, hidden });
                order.push(key.clone());
            }
            let slot = slots.get_mut(&key).unwrap();
            if slot.hidden && op.kind == TxOpKind::Insert {
                return Err(conflict(at(format!("document {} already exists in '{}'.", id, op.tessellation))));
            }
            if matches!(op.kind, TxOpKind::Replace | TxOpKind::Patch | TxOpKind::Delete) && slot.current.as_ref().is_some_and(|d| !write.allows(d)) {
                return Err(EngineError::Forbidden(at(format!("document {} is outside what your role may change in '{}'.", id, op.tessellation))).into());
            }
            if let Some(Value::Object(data)) = &op.data {
                if matches!(op.kind, TxOpKind::Insert | TxOpKind::Replace | TxOpKind::Patch) {
                    write.check_writable(data.keys().map(String::as_str))?;
                }
            }

            if let Some(expected) = op.if_version {
                if slot.written {
                    return Err(invalid(at("if_version only applies to the first operation on a document.".into())));
                }
                let actual = if slot.current.is_some() { slot.base_seq } else { 0 };
                if actual != expected {
                    return Err(conflict(at(format!(
                        "document {} in '{}' is at version {}, not {}.",
                        id, op.tessellation, actual, expected
                    ))));
                }
            }
            if let Some(filter) = &op.if_match {
                match &slot.current {
                    Some(doc) if filter.matches(doc) => {}
                    Some(_) => {
                        return Err(conflict(at(format!(
                            "document {} in '{}' doesn't match if_match.",
                            id, op.tessellation
                        ))))
                    }
                    None => return Err(EngineError::NotFound(at(format!("document {} not found in '{}'.", id, op.tessellation))).into()),
                }
            }

            let missing = || -> anyhow::Error {
                EngineError::NotFound(at(format!("document {} not found in '{}'.", id, op.tessellation))).into()
            };
            let document = match op.kind {
                TxOpKind::Get => Some(slot.current.as_ref().ok_or_else(missing)?.to_api_json()),
                TxOpKind::Check => {
                    if slot.current.is_none() && op.if_version != Some(0) {
                        return Err(missing());
                    }
                    None
                }
                TxOpKind::Insert => {
                    if slot.current.is_some() {
                        return Err(conflict(at(format!("document {} already exists in '{}'.", id, op.tessellation))));
                    }
                    let doc = Document { id, tessellation: op.tessellation.clone(), data: fields(op.data.as_ref().unwrap()), ttl: op.ttl };
                    let json = doc.to_api_json();
                    slot.current = Some(doc);
                    slot.written = true;
                    Some(json)
                }
                TxOpKind::Replace => {
                    let doc = slot.current.as_mut().ok_or_else(missing)?;
                    let old = doc.clone();
                    doc.data = fields(op.data.as_ref().unwrap());
                    write.restore_hidden(doc, &old);
                    if op.ttl.is_some() {
                        doc.ttl = op.ttl;
                    }
                    slot.written = true;
                    Some(doc.to_api_json())
                }
                TxOpKind::Patch => {
                    let doc = slot.current.as_mut().ok_or_else(missing)?;
                    if let Some(Value::Object(changes)) = &op.data {
                        apply_patch(&mut doc.data, changes);
                    }
                    if op.ttl.is_some() {
                        doc.ttl = op.ttl;
                    }
                    slot.written = true;
                    Some(doc.to_api_json())
                }
                TxOpKind::Delete => {
                    slot.current.as_ref().ok_or_else(missing)?;
                    slot.current = None;
                    slot.written = true;
                    None
                }
            };
            let document = document.map(|mut json| {
                read.mask_json(&mut json);
                json
            });
            results.push(TxResult { op: op.kind, tessellation: op.tessellation.clone(), id: id.to_string(), version: None, document });
            result_keys.push(key);
        }

        // Every document written must stay within what the caller may write.
        for (key, slot) in &slots {
            if let (true, Some(doc)) = (slot.written, &slot.current) {
                if !scopes.get(&key.tessellation).is_none_or(|(_, write)| write.allows(doc)) {
                    return Err(EngineError::Forbidden(format!(
                        "document {} would be outside what your role may write in '{}'.",
                        key.id, key.tessellation
                    ))
                    .into());
                }
            }
        }

        // One write per document with its final state; documents only read are checked.
        let mut items = Vec::new();
        let mut checks = Vec::new();
        let mut index_of: HashMap<DocKey, usize> = HashMap::new();
        for key in order {
            let slot = &slots[&key];
            if !slot.written {
                checks.push((key, slot.base_seq));
                continue;
            }
            let op = match &slot.current {
                Some(doc) => WalOp::Put(doc.clone()),
                // Inserted then deleted in the same transaction: nothing to write.
                None if slot.base_seq == 0 => {
                    checks.push((key, slot.base_seq));
                    continue;
                }
                None => WalOp::Delete { tessellation: key.tessellation.clone(), id: key.id },
            };
            index_of.insert(key.clone(), items.len());
            items.push(BatchItem { key, op, expected_seq: Some(slot.base_seq) });
        }

        let write_index = result_keys.iter().map(|k| index_of.get(k).copied()).collect();
        let read_version = result_keys
            .iter()
            .map(|k| {
                let slot = &slots[k];
                (!slot.written && slot.current.is_some()).then_some(slot.base_seq)
            })
            .collect();
        // Before triggers may change or refuse the writes (results show the planned documents).
        self.run_before_triggers(&mut items).await?;
        Ok(Plan { items, checks, results, write_index, read_version })
    }
}

/// Apply a JSON merge-patch at the top level: null removes a field.
fn apply_patch(data: &mut CompactFields, changes: &Map<String, Value>) {
    for (k, v) in changes {
        if is_reserved_field(k) {
            continue;
        }
        if v.is_null() {
            data.remove(k);
        } else {
            data.insert(k.clone(), FieldValue::from_json(v));
        }
    }
}
