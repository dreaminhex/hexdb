// HexDB Core Engine: parent links, cascading deletes, the document trash
//
// A schema field can be a parent link (see `crate::schema::ParentLink`): it
// holds the ID of the document or file that owns this one, and says what
// happens to this document when its parent is deleted. Links mark ownership
// only. HexDB stays a document store: there are no joins over them, and writes
// don't check that a parent exists.
//
// Each link field gets a field index (unless one already starts with it), so
// finding a document's children is an index lookup, not a scan.
//
// The trash: every delete of a user document goes through `trash_deletes`
// inside the commit, which adds, in the same atomic batch, a copy of the
// document to `_trash` (kept `storage.trash_days`) and one record of the
// whole delete to `_deletes` (the delete history, kept
// `storage.delete_history_days`). Everything one commit deleted is one delete
// batch, and `restore_batch` puts all of it back in one commit. Deleted files
// go to the files trash as before (see engine::files); the batch lists them.
//
// A trash entry is `{from, doc_id, batch, deleted_at, deleted_by, expires_at,
// document: {...the stored fields...}}` with a fresh ID. The document's sealed
// fields stay sealed there (under `document.`, see engine::sealing) and its
// tokens stay tokens, and a restore writes it back as it was stored: with its
// `_schema` stamp, without validating it against a newer version.
//
// Cascades: before the trash, `cascade_deletes` follows the parent links from
// every document the commit deletes (breadth-first, so trees and cycles end):
// `cascade` children are deleted too, `detach` children get their link set to
// null (the batch records the old value, so a restore puts it back), and a
// `restrict` child that isn't going anyway refuses the whole commit. All of
// it is one atomic commit; a document the caller may not change refuses it
// too, so a delete never goes through partially. A child inserted at the same
// moment as its parent is deleted can be left behind: inserts don't check
// that their parent exists.
//
// Not deletes, so not trashed: TTL expiry, writes HexDB makes itself (files
// internals, replication), dropping a tessellation, and system tessellations.

use super::{writes::BatchItem, EngineError, HexDBEngine};
use crate::{
    document::{infer_fields_from_json, CompactFields, Document, FieldValue},
    filter::Filter,
    hex::DocKey,
    index::{IndexDef, IndexKind},
    schema::{OnDelete, ParentLink, SchemaInput},
    wal::WalOp,
};
use anyhow::Result;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use tracing::warn;
use ulid::Ulid;

/// Deleted documents, until they're restored, purged or expire.
pub const TRASH_TESSELLATION: &str = "_trash";
/// One record per delete batch: who deleted what, when, and how.
pub const DELETES_TESSELLATION: &str = "_deletes";
/// The field of a trash entry holding the deleted document's fields.
pub const TRASHED_DOCUMENT: &str = "document";

const DAY_MILLIS: i64 = 86_400_000;

/// What a delete was, for its history record (set by the code that deletes).
#[derive(Debug, Clone, Default)]
pub struct DeleteContext {
    /// delete (by ID), filter, transaction, folder; empty: delete.
    pub kind: String,
    /// The filter of a delete by filter (already redacted).
    pub filter: Option<Value>,
}

tokio::task_local! {
    static CONTEXT: DeleteContext;
    static RESTORING: ();
}

/// Run `f` with `context` describing the deletes it commits.
pub async fn with_delete_context<F: Future>(context: DeleteContext, f: F) -> F::Output {
    CONTEXT.scope(context, f).await
}

fn context() -> DeleteContext {
    CONTEXT.try_with(Clone::clone).unwrap_or_default()
}

/// True while a restore commits (documents go back as they were stored).
pub(crate) fn is_restoring() -> bool {
    RESTORING.try_with(|_| ()).is_ok()
}

fn now_text() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn caller_login() -> String {
    crate::access::caller().map(|p| p.login.clone()).unwrap_or_else(|| "system".into())
}

fn string_field(doc: &Document, field: &str) -> Option<String> {
    match doc.data.get(field) {
        Some(FieldValue::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// What restoring a batch did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RestoreSummary {
    pub batch: String,
    /// Documents put back, per tessellation.
    pub restored: BTreeMap<String, usize>,
    /// Documents left out because a document with the same ID exists again.
    pub skipped: Vec<Value>,
    /// Detached links set back to their parent.
    pub relinked: usize,
}

/// A link from a child tessellation's field to a parent tessellation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChildLink {
    /// The tessellation whose documents hold the link.
    pub tessellation: String,
    pub field: String,
    pub on_delete: OnDelete,
}

impl HexDBEngine {
    /// `tess`'s parent links in its current schema version: (field, link).
    pub fn parent_links(&self, tess: &str) -> Vec<(String, ParentLink)> {
        self.schemas(tess)
            .last()
            .map(|v| v.links().map(|(f, l)| (f.clone(), l.clone())).collect())
            .unwrap_or_default()
    }

    /// Every link pointing at `parent` (its own, for trees, included).
    pub fn child_links(&self, parent: &str) -> Vec<ChildLink> {
        let catalog = self.catalog.lock().unwrap();
        let mut out = Vec::new();
        for (tess, info) in &catalog.tessellations {
            let Some(current) = info.schemas.last() else { continue };
            for (field, link) in current.links() {
                if link.tessellation == parent {
                    out.push(ChildLink { tessellation: tess.clone(), field: field.clone(), on_delete: link.on_delete });
                }
            }
        }
        out
    }

    /// Refuse links to tessellations that don't exist or hold no user data.
    pub(crate) fn check_links(&self, tess: &str, input: &SchemaInput) -> Result<()> {
        for (field, rule) in &input.fields {
            let Some(link) = &rule.parent else { continue };
            if link.tessellation == tess {
                continue;
            }
            match self.tessellation_info(&link.tessellation) {
                Some(info) if info.is_user_data() => {}
                Some(_) => return Err(EngineError::Invalid(format!("{}: '{}' is a system tessellation; it can't be a parent.", field, link.tessellation)).into()),
                None => return Err(EngineError::Invalid(format!("{}: the parent tessellation '{}' doesn't exist.", field, link.tessellation)).into()),
            }
        }
        Ok(())
    }

    /// Give each of `tess`'s link fields an index, unless one already starts with it.
    pub(crate) async fn ensure_link_indexes(&self, tess: &str) {
        for (field, _) in self.parent_links(tess) {
            let covered = self
                .list_indexes(tess)
                .iter()
                .any(|i| i.def.kind == IndexKind::Field && i.def.fields.first() == Some(&field));
            if covered {
                continue;
            }
            let def = IndexDef { fields: vec![field.clone()], ..Default::default() };
            if let Err(e) = self.create_index(tess, def).await {
                warn!("⚠️ Couldn't index the parent link '{}' of '{}': {:#}", field, tess, e);
            }
        }
    }

    /// Refuse to drop a tessellation other tessellations link to as a parent.
    pub(crate) fn check_not_parent(&self, tess: &str) -> Result<()> {
        let others: Vec<String> = self
            .child_links(tess)
            .into_iter()
            .filter(|l| l.tessellation != tess)
            .map(|l| format!("{}.{}", l.tessellation, l.field))
            .collect();
        if others.is_empty() {
            return Ok(());
        }
        Err(EngineError::Conflict(format!(
            "'{}' is the parent of {} (parent links in their schemas). Remove those links first.",
            tess,
            others.join(", ")
        ))
        .into())
    }
}

/// What deleting some documents would do, following parent links.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CascadePlan {
    /// Every document and file that goes, per tessellation (the roots included).
    pub deleted: BTreeMap<String, usize>,
    /// Documents that stay with their link set to null, per tessellation.
    pub detached: BTreeMap<String, usize>,
    /// Children whose `restrict` link refuses the delete: per tessellation,
    /// a count and the first few IDs.
    pub blocked: BTreeMap<String, Value>,
    /// A few of the documents that go (tessellation/id), for previews.
    pub sample: Vec<String>,
    #[serde(skip)]
    deletes: Vec<(DocKey, u64)>,
    #[serde(skip)]
    detaches: Vec<(Document, u64, String)>,
}

impl CascadePlan {
    /// Documents and files that go.
    pub fn total(&self) -> usize {
        self.deleted.values().sum()
    }
}

/// Most documents and files one delete may remove (its cascade included).
pub const MAX_DELETE: usize = 100_000;
const SAMPLE: usize = 10;

impl HexDBEngine {
    /// Follow the parent links from `roots` (documents being deleted): the
    /// children that cascade, detach or block, recursively. `written` are
    /// documents the same commit writes (a child written and cascaded in one
    /// request is refused). Fails if the caller may not change a document the
    /// delete would reach, or if it would remove more than MAX_DELETE.
    pub(crate) async fn plan_cascade(&self, roots: &[DocKey], written: &HashSet<DocKey>) -> Result<CascadePlan> {
        let mut plan = CascadePlan::default();
        let mut going: HashSet<DocKey> = roots.iter().cloned().collect();
        let mut detach: HashMap<DocKey, (Document, u64, String)> = HashMap::new();
        let mut blocked: Vec<DocKey> = Vec::new();
        let mut queue: VecDeque<DocKey> = roots.iter().cloned().collect();
        for key in roots {
            *plan.deleted.entry(key.tessellation.clone()).or_default() += 1;
            if plan.sample.len() < SAMPLE {
                plan.sample.push(format!("{}/{}", key.tessellation, key.id));
            }
        }
        let mut links: HashMap<String, Vec<ChildLink>> = HashMap::new();
        while let Some(parent) = queue.pop_front() {
            let children = links.entry(parent.tessellation.clone()).or_insert_with(|| self.child_links(&parent.tessellation)).clone();
            for link in children {
                let filter = Filter::parse(&json!({ link.field.as_str(): parent.id.to_string() }))?;
                for child in self.matching_documents(&link.tessellation, &filter).await? {
                    let key = DocKey::new(&link.tessellation, child.id);
                    if going.contains(&key) {
                        continue;
                    }
                    match link.on_delete {
                        OnDelete::Restrict => blocked.push(key),
                        OnDelete::Cascade => {
                            if written.contains(&key) {
                                return Err(EngineError::Invalid(format!(
                                    "Document {} in '{}' is written in this request and deleted with its parent; do one or the other.",
                                    child.id, link.tessellation
                                ))
                                .into());
                            }
                            self.check_may_change(&link.tessellation, &child, "delete")?;
                            let (_, seq) = self.read_latest(&key).await?;
                            going.insert(key.clone());
                            detach.remove(&key);
                            *plan.deleted.entry(link.tessellation.clone()).or_default() += 1;
                            if plan.sample.len() < SAMPLE {
                                plan.sample.push(format!("{}/{}", link.tessellation, child.id));
                            }
                            if plan.total() > MAX_DELETE {
                                return Err(EngineError::TooLarge(format!(
                                    "The delete would remove more than {} documents and files (with what cascades from them); narrow it.",
                                    MAX_DELETE
                                ))
                                .into());
                            }
                            plan.deletes.push((key.clone(), seq));
                            queue.push_back(key);
                        }
                        OnDelete::Detach => {
                            if detach.contains_key(&key) {
                                continue;
                            }
                            self.check_may_change(&link.tessellation, &child, "change")?;
                            let (_, seq) = self.read_latest(&key).await?;
                            detach.insert(key, (child, seq, link.field.clone()));
                        }
                    }
                }
            }
        }
        // A restricting child that goes anyway (matched, or cascaded another way) doesn't block.
        for key in blocked.into_iter().filter(|k| !going.contains(k)) {
            let entry = plan.blocked.entry(key.tessellation.clone()).or_insert_with(|| json!({ "count": 0, "ids": [] }));
            entry["count"] = json!(entry["count"].as_u64().unwrap_or(0) + 1);
            if let Some(ids) = entry["ids"].as_array_mut().filter(|ids| ids.len() < 5) {
                ids.push(json!(key.id.to_string()));
            }
        }
        for (key, (doc, seq, field)) in detach {
            if going.contains(&key) {
                continue;
            }
            *plan.detached.entry(key.tessellation.clone()).or_default() += 1;
            plan.detaches.push((doc, seq, field));
        }
        Ok(plan)
    }

    /// Refuse a cascade that reaches a document the caller may not change.
    fn check_may_change(&self, tess: &str, doc: &Document, what: &str) -> Result<()> {
        let Some(principal) = crate::access::caller() else { return Ok(()) };
        let files = self.tessellation_info(tess).is_some_and(|t| t.is_files());
        let permission = if files && what == "delete" { crate::auth::Permission::Delete } else { crate::auth::Permission::Write };
        let action = if files && what == "delete" { crate::auth::Action::Delete } else { crate::auth::Action::Write };
        if !principal.can(permission, tess) || !principal.scope(tess, action).allows(doc) {
            return Err(EngineError::Forbidden(format!(
                "This delete would also {} documents in '{}' (through parent links) that your role may not {}; nothing was deleted.",
                what, tess, what
            ))
            .into());
        }
        Ok(())
    }

    /// Inside a commit: add what cascades from the deletes in `items`, or
    /// refuse the commit if a restricting child would be left behind.
    async fn cascade_deletes(&self, items: &mut Vec<BatchItem>) -> Result<Vec<Value>> {
        let roots: Vec<DocKey> = items
            .iter()
            .filter(|i| matches!(i.op, WalOp::Delete { .. }) && self.tessellation_info(&i.key.tessellation).is_some_and(|t| t.is_user_data()))
            .map(|i| i.key.clone())
            .collect();
        if roots.is_empty() || !roots.iter().any(|k| !self.child_links(&k.tessellation).is_empty()) {
            return Ok(Vec::new());
        }
        let written: HashSet<DocKey> = items.iter().filter(|i| matches!(i.op, WalOp::Put(_))).map(|i| i.key.clone()).collect();
        let plan = self.plan_cascade(&roots, &written).await?;
        if !plan.blocked.is_empty() {
            return Err(blocked_error(&plan).into());
        }
        let mut added = Vec::new();
        for (key, seq) in plan.deletes {
            let op = WalOp::Delete { tessellation: key.tessellation.clone(), id: key.id };
            added.push(BatchItem { key, op, expected_seq: Some(seq) });
        }
        let mut detached = Vec::new();
        for (mut doc, seq, field) in plan.detaches {
            let key = DocKey::new(&doc.tessellation, doc.id);
            let parent = doc.data_json().get(&field).cloned().unwrap_or(Value::Null);
            detached.push(json!({ "tessellation": doc.tessellation, "id": doc.id.to_string(), "field": field, "value": parent }));
            if let Some(existing) = items.iter_mut().find(|i| i.key == key) {
                // Written in this request too: set the link to null in that write.
                if let WalOp::Put(written) = &mut existing.op {
                    set_null(written, &field);
                }
                continue;
            }
            set_null(&mut doc, &field);
            added.push(BatchItem { key, op: WalOp::Put(doc), expected_seq: Some(seq) });
        }
        self.run_before_triggers(&mut added).await?;
        items.extend(added);
        Ok(detached)
    }
}

/// The 409 for a delete that restricting children refuse.
pub fn blocked_error(plan: &CascadePlan) -> EngineError {
    let list: Vec<String> = plan
        .blocked
        .iter()
        .map(|(tess, b)| format!("{} in '{}' ({})", b["count"], tess, b["ids"].as_array().map(|ids| ids.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default()))
        .collect();
    EngineError::Conflict(format!(
        "Documents link to what you're deleting with on_delete restrict: {}. Delete or move them first; nothing was deleted.",
        list.join("; ")
    ))
}

/// Set a (possibly dotted) field to null.
fn set_null(doc: &mut Document, field: &str) {
    let mut json = doc.data_json();
    set_path(&mut json, field, Value::Null);
    doc.data = infer_fields_from_json(&json);
}

/// A (possibly dotted) field of a JSON object.
fn get_path<'a>(json: &'a Value, field: &str) -> Option<&'a Value> {
    field.split('.').try_fold(json, |node, part| node.get(part))
}

/// Set a (possibly dotted) field of a JSON object, if its parents are objects.
fn set_path(json: &mut Value, field: &str, value: Value) {
    let mut node = json;
    let parts: Vec<&str> = field.split('.').collect();
    for part in &parts[..parts.len() - 1] {
        match node.get_mut(*part) {
            Some(next) if next.is_object() => node = next,
            _ => return,
        }
    }
    if let Some(map) = node.as_object_mut() {
        map.insert(parts[parts.len() - 1].to_string(), value);
    }
}

impl HexDBEngine {
    /// Inside a commit: trash the user documents `items` delete and record
    /// the batch. Returns the batch ID, if the commit deletes any.
    pub(crate) async fn trash_deletes(&self, items: &mut Vec<BatchItem>) -> Result<Option<Ulid>> {
        if is_restoring() || crate::files::is_internal() {
            return Ok(None);
        }
        let detached = self.cascade_deletes(items).await?;
        let deletes: Vec<DocKey> = items
            .iter()
            .filter(|i| matches!(i.op, WalOp::Delete { .. }) && self.tessellation_info(&i.key.tessellation).is_some_and(|t| t.is_user_data()))
            .map(|i| i.key.clone())
            .collect();
        if deletes.is_empty() {
            return Ok(None);
        }
        let batch = Ulid::generate();
        let now = Utc::now().timestamp_millis();
        let (at, by) = (now_text(), caller_login());
        let trash_days = self.config.storage.trash_days as i64;
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut files = Vec::new();
        let mut added = Vec::new();
        for key in &deletes {
            let (Some(previous), _) = self.read_latest(key).await? else { continue };
            *counts.entry(key.tessellation.clone()).or_default() += 1;
            if self.tessellation_info(&key.tessellation).is_some_and(|t| t.is_files()) {
                // The files trash keeps it (engine::files); the batch remembers where.
                files.push(json!({ "tessellation": key.tessellation, "id": key.id.to_string() }));
                continue;
            }
            if trash_days == 0 {
                continue;
            }
            let mut data: CompactFields = BTreeMap::new();
            data.insert("from".into(), FieldValue::String(key.tessellation.clone()));
            data.insert("doc_id".into(), FieldValue::String(key.id.to_string()));
            data.insert("batch".into(), FieldValue::String(batch.to_string()));
            data.insert("deleted_at".into(), FieldValue::String(at.clone()));
            data.insert("deleted_by".into(), FieldValue::String(by.clone()));
            if let Some(expires) = previous.ttl {
                data.insert("expires_at".into(), FieldValue::Integer(expires));
            }
            data.insert(TRASHED_DOCUMENT.into(), FieldValue::Object(previous.data));
            let entry = Document { id: Ulid::generate(), tessellation: TRASH_TESSELLATION.into(), data, ttl: Some(now + trash_days * DAY_MILLIS) };
            added.push(BatchItem { key: DocKey::new(TRASH_TESSELLATION, entry.id), op: WalOp::Put(entry), expected_seq: None });
        }
        if counts.is_empty() {
            return Ok(None);
        }
        let ctx = context();
        let kind = if ctx.kind.is_empty() { "delete".to_string() } else { ctx.kind };
        let record = json!({
            "at": at,
            "by": by,
            "kind": kind,
            "tessellation": deletes[0].tessellation,
            "filter": ctx.filter,
            "counts": counts,
            "documents": counts.values().sum::<usize>() - files.len(),
            "files": files,
            "detached": detached,
            "trashed": trash_days > 0,
        });
        let history_days = self.config.storage.delete_history_days.max(1) as i64;
        let record = Document { id: batch, tessellation: DELETES_TESSELLATION.into(), data: infer_fields_from_json(&record), ttl: Some(now + history_days * DAY_MILLIS) };
        added.push(BatchItem { key: DocKey::new(DELETES_TESSELLATION, batch), op: WalOp::Put(record), expected_seq: Some(0) });
        self.ensure_trash().await?;
        items.extend(added);
        Ok(Some(batch))
    }

    /// Create the trash and the delete history, with their indexes.
    async fn ensure_trash(&self) -> Result<()> {
        if self.tessellation_exists(TRASH_TESSELLATION) && self.tessellation_exists(DELETES_TESSELLATION) {
            return Ok(());
        }
        self.ensure_system_tessellation(TRASH_TESSELLATION)?;
        self.ensure_system_tessellation(DELETES_TESSELLATION)?;
        for field in ["batch", "from"] {
            self.ensure_internal_index(TRASH_TESSELLATION, IndexDef { fields: vec![field.into()], ..Default::default() }).await?;
        }
        Ok(())
    }

    /// A delete batch's history record.
    pub async fn delete_batch(&self, batch: &str) -> Result<Option<Document>> {
        let Ok(id) = Ulid::from_string(batch) else { return Ok(None) };
        if !self.tessellation_exists(DELETES_TESSELLATION) {
            return Ok(None);
        }
        Ok(self.read_latest(&DocKey::new(DELETES_TESSELLATION, id)).await?.0)
    }

    /// The trash entries of a batch.
    pub async fn trashed_in_batch(&self, batch: &str) -> Result<Vec<Document>> {
        if !self.tessellation_exists(TRASH_TESSELLATION) {
            return Ok(Vec::new());
        }
        self.matching_documents(TRASH_TESSELLATION, &Filter::parse(&json!({ "batch": batch }))?).await
    }

    /// The document a trash entry holds, as it was stored.
    pub fn trashed_document(entry: &Document) -> Option<Document> {
        let tessellation = string_field(entry, "from")?;
        let id = Ulid::from_string(&string_field(entry, "doc_id")?).ok()?;
        let data = match entry.data.get(TRASHED_DOCUMENT) {
            Some(FieldValue::Object(fields)) => fields.clone(),
            _ => BTreeMap::new(),
        };
        let ttl = match entry.data.get("expires_at") {
            Some(FieldValue::Integer(at)) => Some(*at),
            _ => None,
        };
        Some(Document { id, tessellation, data, ttl })
    }

    /// Put back everything a delete batch removed, in one commit. Documents
    /// whose IDs exist again are a conflict, or left out with `skip_existing`.
    /// The caller needs write access to every document it puts back.
    pub async fn restore_batch(&self, batch: &str, skip_existing: bool) -> Result<RestoreSummary> {
        self.ensure_writable()?;
        for _ in 0..super::MAX_WRITE_RETRIES {
            let (record, record_seq) = match Ulid::from_string(batch) {
                Ok(id) if self.tessellation_exists(DELETES_TESSELLATION) => self.read_latest(&DocKey::new(DELETES_TESSELLATION, id)).await?,
                _ => (None, 0),
            };
            let record = record.ok_or_else(|| EngineError::NotFound(format!("There's no delete batch {}.", batch)))?;
            let fields = record.data_json();
            if let Some(at) = fields.get("restored_at").and_then(Value::as_str) {
                return Err(EngineError::Conflict(format!("Delete batch {} was already restored ({}).", batch, at)).into());
            }
            if fields.get("purged_at").and_then(Value::as_str).is_some() || fields["trashed"] == false {
                return Err(EngineError::Conflict(format!("Delete batch {} can't be restored: its documents weren't kept or were purged.", batch)).into());
            }

            let mut summary = RestoreSummary { batch: batch.to_string(), ..Default::default() };
            let mut items = Vec::new();
            let mut existing = Vec::new();
            for entry in self.trashed_in_batch(batch).await? {
                let (entry_now, entry_seq) = self.read_latest(&DocKey::new(TRASH_TESSELLATION, entry.id)).await?;
                if entry_now.is_none() {
                    continue;
                }
                let Some(doc) = Self::trashed_document(&entry) else { continue };
                if !self.tessellation_info(&doc.tessellation).is_some_and(|t| t.is_user_data()) {
                    return Err(EngineError::Conflict(format!("The tessellation '{}' no longer exists; create it to restore batch {}.", doc.tessellation, batch)).into());
                }
                if !self.caller_scope(&doc.tessellation, crate::auth::Action::Write).allows(&doc) {
                    return Err(EngineError::Forbidden(format!("Your role may not restore documents in '{}'.", doc.tessellation)).into());
                }
                let key = DocKey::new(&doc.tessellation, doc.id);
                let (current, seq) = self.read_latest(&key).await?;
                if current.is_some() {
                    existing.push(json!({ "tessellation": doc.tessellation, "id": doc.id.to_string() }));
                    continue;
                }
                *summary.restored.entry(doc.tessellation.clone()).or_default() += 1;
                items.push(BatchItem { key: DocKey::new(TRASH_TESSELLATION, entry.id), op: WalOp::Delete { tessellation: TRASH_TESSELLATION.into(), id: entry.id }, expected_seq: Some(entry_seq) });
                items.push(BatchItem { key, op: WalOp::Put(doc), expected_seq: Some(seq) });
            }
            if !existing.is_empty() && !skip_existing {
                let list: Vec<String> = existing.iter().take(10).map(|e| format!("{}/{}", e["tessellation"].as_str().unwrap_or(""), e["id"].as_str().unwrap_or(""))).collect();
                return Err(EngineError::Conflict(format!(
                    "{} document(s) of batch {} exist again ({}). Restore the rest with skip_existing=true, or delete those first.",
                    existing.len(),
                    batch,
                    list.join(", ")
                ))
                .into());
            }
            summary.skipped = existing;
            for link in fields["detached"].as_array().cloned().unwrap_or_default() {
                let (Some(tess), Some(id), Some(field)) = (link["tessellation"].as_str(), link["id"].as_str().and_then(|id| Ulid::from_string(id).ok()), link["field"].as_str()) else { continue };
                let key = DocKey::new(tess, id);
                if items.iter().any(|i| i.key == key) {
                    continue;
                }
                let (Some(doc), seq) = self.read_latest(&key).await? else { continue };
                let mut json = doc.data_json();
                if !get_path(&json, field).is_none_or(Value::is_null) {
                    continue;
                }
                set_path(&mut json, field, link["value"].clone());
                let doc = Document { data: infer_fields_from_json(&json), ..doc };
                items.push(BatchItem { key, op: WalOp::Put(doc), expected_seq: Some(seq) });
                summary.relinked += 1;
            }

            let mut map = match fields {
                Value::Object(map) => map,
                _ => Map::new(),
            };
            map.insert("restored_at".into(), json!(now_text()));
            map.insert("restored_by".into(), json!(caller_login()));
            map.insert("restored".into(), json!(summary.restored));
            let record = Document { id: record.id, tessellation: DELETES_TESSELLATION.into(), data: infer_fields_from_json(&Value::Object(map)), ttl: record.ttl };
            items.push(BatchItem { key: DocKey::new(DELETES_TESSELLATION, record.id), op: WalOp::Put(record), expected_seq: Some(record_seq) });

            if RESTORING.scope((), self.commit(items)).await? {
                return Ok(summary);
            }
        }
        Err(EngineError::Conflict("The documents are being modified concurrently; try again.".into()).into())
    }
}

#[cfg(test)]
mod tests {
    use crate::schema::{new_version, OnDelete, SchemaInput};
    use serde_json::json;

    fn input(v: serde_json::Value) -> SchemaInput {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn link_rules() {
        let ok = new_version(&[], input(json!({ "fields": { "order_id": { "type": "string", "required": true, "parent": { "tessellation": "orders", "on_delete": "cascade" } } } }))).unwrap();
        let links: Vec<_> = ok.links().collect();
        assert_eq!(links.len(), 1);
        assert_eq!((links[0].0.as_str(), links[0].1.on_delete), ("order_id", OnDelete::Cascade));

        let refused = |rule: serde_json::Value| new_version(&[], input(json!({ "fields": { "p": rule } }))).unwrap_err().to_string();
        assert!(refused(json!({ "type": "integer", "parent": { "tessellation": "orders", "on_delete": "cascade" } })).contains("string field"));
        assert!(refused(json!({ "type": "string", "tokenize": "one_way", "parent": { "tessellation": "orders", "on_delete": "cascade" } })).contains("tokenized"));
        assert!(refused(json!({ "type": "string", "required": true, "parent": { "tessellation": "orders", "on_delete": "detach" } })).contains("nullable"));
        assert!(refused(json!({ "type": "string", "parent": { "tessellation": "_users", "on_delete": "cascade" } })).contains("parent tessellation"));
        // on_delete is required: no silent default for what a delete does.
        assert!(serde_json::from_value::<SchemaInput>(json!({ "fields": { "p": { "parent": { "tessellation": "orders" } } } })).is_err());
        // Detach on a nullable required field is fine.
        new_version(&[], input(json!({ "fields": { "p": { "type": "string", "required": true, "nullable": true, "parent": { "tessellation": "orders", "on_delete": "detach" } } } }))).unwrap();
    }

    use super::{DELETES_TESSELLATION, TRASH_TESSELLATION};
    use crate::{engine::writes::BatchItem, hex::DocKey, users, wal::WalOp, Document, EngineError};

    fn batch_of(engine: &super::HexDBEngine) -> String {
        let ids = engine.state.try_lock().map(|s| s.hex.keys()).unwrap_or_default();
        let mut batches: Vec<String> = ids.into_iter().filter(|k| k.tessellation == DELETES_TESSELLATION).map(|k| k.id.to_string()).collect();
        batches.sort();
        batches.pop().expect("a delete batch")
    }

    fn kind(e: &anyhow::Error) -> Option<&EngineError> {
        e.downcast_ref::<EngineError>()
    }

    #[tokio::test]
    async fn deletes_go_to_the_trash_and_restore_as_they_were() {
        let (engine, dir) = crate::engine::test_engine().await;
        let doc = engine.insert_json("orders", json!({ "customer": "ada", "total": 12.5, "lines": [{ "sku": "A" }] }), Some(4_102_444_800_000)).await.unwrap();
        let id = doc.id.to_string();
        assert!(engine.delete_document("orders", &id, None).await.unwrap().value);
        assert!(engine.get_document("orders", &id).await.unwrap().is_none());

        // One trash entry and one history record.
        let batch = batch_of(&engine);
        let record = engine.delete_batch(&batch).await.unwrap().unwrap().data_json();
        assert_eq!((record["kind"].as_str(), record["documents"].as_u64(), record["counts"]["orders"].as_u64()), (Some("delete"), Some(1), Some(1)), "{}", record);
        assert_eq!(record["by"], "system");
        let trashed = engine.trashed_in_batch(&batch).await.unwrap();
        assert_eq!(trashed.len(), 1);
        let entry = trashed[0].data_json();
        assert_eq!((entry["from"].as_str(), entry["doc_id"].as_str(), entry["document"]["customer"].as_str()), (Some("orders"), Some(id.as_str()), Some("ada")));
        assert!(trashed[0].ttl.is_some(), "trash entries expire");

        // Restore puts back the same document (ID, fields, expiry) and empties the batch.
        let summary = engine.restore_batch(&batch, false).await.unwrap();
        assert_eq!(summary.restored.get("orders"), Some(&1));
        let back = engine.get_document("orders", &id).await.unwrap().unwrap();
        assert_eq!(back.data_json(), doc.data_json());
        assert_eq!(back.ttl, Some(4_102_444_800_000));
        assert!(engine.trashed_in_batch(&batch).await.unwrap().is_empty());
        let record = engine.delete_batch(&batch).await.unwrap().unwrap().data_json();
        assert!(record["restored_at"].is_string(), "{}", record);
        let again = engine.restore_batch(&batch, false).await.unwrap_err();
        assert!(matches!(kind(&again), Some(EngineError::Conflict(_))), "{:#}", again);
        assert!(matches!(kind(&engine.restore_batch("01J00000000000000000000000", false).await.unwrap_err()), Some(EngineError::NotFound(_))));

        // A document whose ID exists again blocks the restore, unless skipped.
        assert!(engine.delete_document("orders", &id, None).await.unwrap().value);
        let batch = batch_of(&engine);
        let twin = Document { id: doc.id, tessellation: "orders".into(), data: doc.data.clone(), ttl: None };
        assert!(engine.commit(vec![BatchItem { key: DocKey::new("orders", doc.id), op: WalOp::Put(twin), expected_seq: None }]).await.unwrap());
        let blocked = engine.restore_batch(&batch, false).await.unwrap_err();
        assert!(blocked.to_string().contains("exist again"), "{:#}", blocked);
        let summary = engine.restore_batch(&batch, true).await.unwrap();
        assert_eq!((summary.restored.len(), summary.skipped.len()), (0, 1));

        // Deleting from a system tessellation isn't trashed.
        let entries = engine.count_documents(TRASH_TESSELLATION).await.unwrap();
        let record_id = DocKey::new(DELETES_TESSELLATION, ulid::Ulid::from_string(&batch).unwrap());
        let op = WalOp::Delete { tessellation: DELETES_TESSELLATION.into(), id: record_id.id };
        assert!(engine.commit(vec![BatchItem { key: record_id, op, expected_seq: None }]).await.unwrap());
        assert_eq!(engine.count_documents(TRASH_TESSELLATION).await.unwrap(), entries);
        engine.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn masked_fields_stay_sealed_in_the_trash() {
        const CARD: &str = "4111111111111111";
        let (engine, dir) = crate::engine::test_engine().await;
        engine.create_tessellation(users::ROLES_TESSELLATION, "system").unwrap();
        users::create_role(
            &engine,
            users::RoleInput {
                name: Some("clerk".into()),
                description: None,
                permissions: Some(vec!["read".into()]),
                restrictions: Some(serde_json::from_value(json!({ "orders": { "mask": { "card": { "show_last": 4 } } } })).unwrap()),
            },
        )
        .await
        .unwrap();
        let id = engine.insert_json("orders", json!({ "card": CARD, "name": "Ada" }), None).await.unwrap().id;
        engine.delete_document("orders", &id.to_string(), None).await.unwrap();
        let batch = batch_of(&engine);
        let entry = engine.trashed_in_batch(&batch).await.unwrap().remove(0);

        // In memory the trash entry holds the card sealed; reading it opens it.
        let held = match engine.state.lock().await.hex.read(&DocKey::new(TRASH_TESSELLATION, entry.id)) {
            Some(crate::hex::Lookup::Live { bytes, .. }) => String::from_utf8_lossy(&bytes).into_owned(),
            _ => String::new(),
        };
        assert!(!held.contains(CARD) && held.contains(crate::sealed::MARK), "{}", held);
        assert_eq!(entry.data_json()["document"]["card"], CARD);

        engine.restore_batch(&batch, false).await.unwrap();
        assert_eq!(engine.get_document("orders", &id.to_string()).await.unwrap().unwrap().data_json()["card"], CARD);
        engine.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    fn principal(name: &str, permissions: Vec<crate::auth::Action>, tessellations: &[&str]) -> crate::auth::Principal {
        let mut defs = crate::auth::RoleDefinitions::builtin();
        defs.0.insert(name.into(), crate::auth::RoleRules { permissions, restrictions: Default::default() });
        let grants = vec![crate::users::RoleGrant { name: name.into(), tessellations: tessellations.iter().map(|t| t.to_string()).collect() }];
        crate::auth::Principal::new("u1".into(), "ada".into(), "ada@example.com".into(), grants, crate::auth::Credential::ApiKey { key_id: "k".into() }, &defs)
    }

    async fn link(engine: &super::HexDBEngine, tess: &str, fields: serde_json::Value) {
        engine.create_tessellation(tess, "user").unwrap();
        let admin = principal("schema-admin", vec![crate::auth::Action::Manage], &["*"]);
        engine.add_schema(tess, input(json!({ "fields": fields })), &admin).await.unwrap();
    }

    async fn exists(engine: &super::HexDBEngine, tess: &str, id: &ulid::Ulid) -> bool {
        engine.get_document(tess, &id.to_string()).await.unwrap().is_some()
    }

    #[tokio::test]
    async fn deletes_cascade_detach_and_restrict_along_parent_links() {
        let (engine, dir) = crate::engine::test_engine().await;
        engine.create_tessellation("orders", "user").unwrap();
        let cascade = |t: &str| json!({ "type": "string", "parent": { "tessellation": t, "on_delete": "cascade" } });
        link(&engine, "items", json!({ "order_id": cascade("orders") })).await;
        link(&engine, "item_notes", json!({ "item_id": cascade("items") })).await;
        link(&engine, "notes", json!({ "order_id": { "type": "string", "nullable": true, "parent": { "tessellation": "orders", "on_delete": "detach" } } })).await;
        link(&engine, "invoices", json!({ "order_id": { "type": "string", "parent": { "tessellation": "orders", "on_delete": "restrict" } } })).await;

        let order = engine.insert_json("orders", json!({ "n": 1 }), None).await.unwrap().id;
        let o = order.to_string();
        let item1 = engine.insert_json("items", json!({ "order_id": o }), None).await.unwrap().id;
        let item2 = engine.insert_json("items", json!({ "order_id": o }), None).await.unwrap().id;
        let deep = engine.insert_json("item_notes", json!({ "item_id": item1.to_string() }), None).await.unwrap().id;
        let note = engine.insert_json("notes", json!({ "order_id": o, "text": "call" }), None).await.unwrap().id;
        let other = engine.insert_json("items", json!({ "order_id": "01J00000000000000000000000" }), None).await.unwrap().id;

        // Cascades two levels down; detaches the note; leaves unrelated documents alone.
        assert!(engine.delete_document("orders", &o, None).await.unwrap().value);
        for (t, id) in [("orders", order), ("items", item1), ("items", item2), ("item_notes", deep)] {
            assert!(!exists(&engine, t, &id).await, "{} {} should be gone", t, id);
        }
        assert!(exists(&engine, "items", &other).await);
        let detached = engine.get_document("notes", &note.to_string()).await.unwrap().unwrap().data_json();
        assert!(detached["order_id"].is_null() && detached["text"] == "call", "{}", detached);

        // One batch, which restores everything and relinks the note.
        let batch = batch_of(&engine);
        let record = engine.delete_batch(&batch).await.unwrap().unwrap().data_json();
        assert_eq!(record["counts"], json!({ "orders": 1, "items": 2, "item_notes": 1 }), "{}", record);
        assert_eq!(record["detached"][0]["value"], o.as_str());
        let summary = engine.restore_batch(&batch, false).await.unwrap();
        assert_eq!(summary.relinked, 1);
        for (t, id) in [("orders", order), ("items", item1), ("items", item2), ("item_notes", deep)] {
            assert!(exists(&engine, t, &id).await, "{} {} should be back", t, id);
        }
        assert_eq!(engine.get_document("notes", &note.to_string()).await.unwrap().unwrap().data_json()["order_id"], o.as_str());

        // A restricting child refuses the delete, and nothing goes.
        let invoice = engine.insert_json("invoices", json!({ "order_id": o }), None).await.unwrap().id;
        let refused = engine.delete_document("orders", &o, None).await.unwrap_err();
        assert!(matches!(kind(&refused), Some(EngineError::Conflict(_))) && refused.to_string().contains("invoices"), "{:#}", refused);
        assert!(exists(&engine, "orders", &order).await && exists(&engine, "items", &item1).await);
        // Unless it goes in the same request.
        let ops = crate::engine::parse_transaction(&json!({ "operations": [
            { "op": "delete", "tessellation": "invoices", "id": invoice.to_string() },
            { "op": "delete", "tessellation": "orders", "id": o },
        ] }))
        .unwrap();
        engine.transaction(&ops, None).await.unwrap();
        assert!(!exists(&engine, "orders", &order).await && !exists(&engine, "items", &item1).await);

        // The parent's tessellation can't be dropped while others link to it.
        assert!(engine.delete_tessellation("orders").await.is_err());
        engine.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn trees_and_cycles_end_and_permissions_are_all_or_nothing() {
        let (engine, dir) = crate::engine::test_engine().await;
        link(&engine, "folders", json!({ "parent_id": { "type": "string", "parent": { "tessellation": "folders", "on_delete": "cascade" } } })).await;
        let root = engine.insert_json("folders", json!({ "name": "root" }), None).await.unwrap().id;
        let child = engine.insert_json("folders", json!({ "parent_id": root.to_string() }), None).await.unwrap().id;
        let grandchild = engine.insert_json("folders", json!({ "parent_id": child.to_string() }), None).await.unwrap().id;
        // A cycle: root points at the grandchild.
        let patch = vec![(root.to_string(), json!({ "parent_id": grandchild.to_string() }))];
        engine.patch_documents("folders", patch, None, None).await.unwrap();
        let keep = engine.insert_json("folders", json!({ "name": "other" }), None).await.unwrap().id;
        engine.delete_document("folders", &child.to_string(), None).await.unwrap();
        for id in [root, child, grandchild] {
            assert!(!exists(&engine, "folders", &id).await);
        }
        assert!(exists(&engine, "folders", &keep).await);

        // A caller who may change orders but not items can't delete an order with items.
        engine.create_tessellation("orders", "user").unwrap();
        link(&engine, "items", json!({ "order_id": { "type": "string", "parent": { "tessellation": "orders", "on_delete": "cascade" } } })).await;
        let order = engine.insert_json("orders", json!({}), None).await.unwrap().id;
        let item = engine.insert_json("items", json!({ "order_id": order.to_string() }), None).await.unwrap().id;
        let clerk = std::sync::Arc::new(principal("clerk", vec![crate::auth::Action::Read, crate::auth::Action::Write], &["orders"]));
        let refused = crate::access::as_caller(clerk, engine.delete_document("orders", &order.to_string(), None)).await.unwrap_err();
        assert!(matches!(kind(&refused), Some(EngineError::Forbidden(_))), "{:#}", refused);
        assert!(exists(&engine, "orders", &order).await && exists(&engine, "items", &item).await);
        engine.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
