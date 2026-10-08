// HexDB Core Engine: schemas (see `crate::schema`)
//
// Registering a version saves it in the catalog and starts a background
// migration on the Overseer: every document written with an older version (or
// before there was a schema) is migrated step by step to the current version,
// filled with defaults, validated and rewritten with its new `_schema`. A
// document that doesn't fit the new version is left as it is and reported.
// Each write is checked against the current version inside the commit. A
// document from the client is taken to be in the current shape; a stored
// document being patched is upgraded first if it is still on an older version.
//
// Migration steps run in order over a batch of documents: the built-in steps
// on each document, a function step as one call for the whole batch (the
// function gets `documents` and returns them migrated; it runs as whoever
// registered the version). A rollback registers a new version that restores
// an earlier one's fields with the inverse steps (see `schema::rollback_input`).

use super::{writes::BatchItem, EngineError, HexDBEngine};
use crate::{
    document::{CompactFields, FieldValue},
    hex::DocKey,
    schema::{self, SchemaInput, SchemaVersion, SCHEMA_FIELD},
    wal::WalOp,
};
use anyhow::Result;
use serde::Serialize;
use serde_json::{Map, Value};
use std::sync::Arc;
use tracing::{info, warn};

/// Documents migrated per commit.
const MIGRATION_BATCH: usize = 200;

/// A migration's progress, for `GET /tessellations/{name}/schemas`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MigrationStatus {
    pub version: u32,
    /// running, done, or failed (some documents don't fit and were left as they were).
    pub state: String,
    pub migrated: u64,
    pub unchanged: u64,
    pub failed: u64,
    /// The first documents that didn't fit, with why.
    pub errors: Vec<Value>,
    pub started: i64,
    pub finished: Option<i64>,
}

fn version_of(data: &CompactFields) -> u32 {
    match data.get(SCHEMA_FIELD) {
        Some(FieldValue::Integer(v)) => (*v).max(0) as u32,
        _ => 0,
    }
}

fn to_map(data: &CompactFields) -> Map<String, Value> {
    data.iter().filter(|(k, _)| k.as_str() != SCHEMA_FIELD).map(|(k, v)| (k.clone(), v.to_json())).collect()
}

fn from_map(map: &Map<String, Value>, version: u32) -> CompactFields {
    let mut data: CompactFields = map.iter().map(|(k, v)| (k.clone(), FieldValue::from_json(v))).collect();
    data.insert(SCHEMA_FIELD.into(), FieldValue::Integer(version as i64));
    data
}


impl HexDBEngine {
    /// A tessellation's schema versions, oldest first.
    pub fn schemas(&self, tess: &str) -> Vec<SchemaVersion> {
        self.catalog.lock().unwrap().tessellations.get(tess).map(|i| i.schemas.clone()).unwrap_or_default()
    }

    /// The current migration (or the last one), if any.
    pub fn schema_migration(&self, tess: &str) -> Option<MigrationStatus> {
        self.schema_jobs.lock().unwrap().get(tess).cloned()
    }

    /// Register a new schema version (see `crate::schema` for the rules), by
    /// `by` (whom its migration functions run as). The caller starts the
    /// migration with [`HexDBEngine::migrate_schema`].
    pub async fn add_schema(&self, tess: &str, input: SchemaInput, by: &crate::auth::Principal) -> Result<SchemaVersion> {
        self.register_schema(tess, input, by, None).await
    }

    /// Roll back to version `to`: register a new version with its fields and
    /// the inverse migration (plus `extra` steps); see `schema::rollback_input`.
    pub async fn rollback_schema(&self, tess: &str, to: u32, extra: Vec<schema::Step>, by: &crate::auth::Principal) -> Result<SchemaVersion> {
        let input = schema::rollback_input(&self.schemas(tess), to, extra).map_err(|e| EngineError::Conflict(format!("{:#}", e)))?;
        self.register_schema(tess, input, by, Some(to)).await
    }

    async fn register_schema(&self, tess: &str, input: SchemaInput, by: &crate::auth::Principal, restores: Option<u32>) -> Result<SchemaVersion> {
        self.ensure_writable()?;
        for step in &input.migration {
            if let schema::Step::Function { name, undo } = step {
                for f in std::iter::once(name).chain(undo.iter()) {
                    if self.get_function(f).await?.is_none() {
                        return Err(EngineError::Invalid(format!("migration: function '{}' doesn't exist.", f)).into());
                    }
                }
            }
        }
        let mut catalog = self.catalog.lock().unwrap();
        let info = catalog.tessellations.get_mut(tess).ok_or_else(|| EngineError::NotFound(format!("Tessellation '{}' not found.", tess)))?;
        if info.kind != "user" {
            return Err(EngineError::Invalid(format!("'{}' is a system tessellation; it can't have a schema.", tess)).into());
        }
        let mut version = schema::new_version(&info.schemas, input).map_err(|e| EngineError::Conflict(format!("{:#}", e)))?;
        version.created_by = by.user_id.clone();
        version.created_by_login = by.login.clone();
        version.restores = restores;
        info.schemas.push(version.clone());
        catalog.save(&self.storage_dir, &self.keys)?;
        info!("📐 Schema version {} registered for '{}'.", version.version, tess);
        Ok(version)
    }

    /// Remove every schema: the tessellation becomes schemaless again
    /// (documents keep their fields and `_schema` stamps).
    pub fn drop_schemas(&self, tess: &str) -> Result<bool> {
        self.ensure_writable()?;
        let mut catalog = self.catalog.lock().unwrap();
        let Some(info) = catalog.tessellations.get_mut(tess) else { return Ok(false) };
        if info.schemas.is_empty() {
            return Ok(false);
        }
        info.schemas.clear();
        catalog.save(&self.storage_dir, &self.keys)?;
        self.schema_jobs.lock().unwrap().remove(tess);
        Ok(true)
    }

    /// Replace a tessellation's schemas with the Overseer's (replication).
    pub(crate) fn set_schemas_unchecked(&self, tess: &str, schemas: &[SchemaVersion]) -> Result<()> {
        let mut catalog = self.catalog.lock().unwrap();
        if let Some(info) = catalog.tessellations.get_mut(tess) {
            if info.schemas != schemas {
                info.schemas = schemas.to_vec();
                catalog.save(&self.storage_dir, &self.keys)?;
            }
        }
        Ok(())
    }

    /// Migrate documents (each with the version it's on) to the latest of
    /// `versions`, in order. Fails if a migration function fails.
    /// (A boxed future: a migration function may write, and writes migrate.)
    pub fn migrate_maps<'a>(&'a self, tess: &'a str, versions: &'a [SchemaVersion], mut docs: Vec<(u32, Map<String, Value>)>) -> futures::future::BoxFuture<'a, Result<Vec<Map<String, Value>>>> {
        Box::pin(async move {
        for version in versions {
            let due: Vec<usize> = (0..docs.len()).filter(|&i| docs[i].0 < version.version).collect();
            if due.is_empty() {
                continue;
            }
            for step in &version.migration {
                match step {
                    schema::Step::Function { name, .. } => {
                        let batch: Vec<Map<String, Value>> = due.iter().map(|&i| docs[i].1.clone()).collect();
                        let migrated = self.run_migration_function(tess, version, name, batch).await?;
                        for (&i, map) in due.iter().zip(migrated) {
                            docs[i].1 = map;
                        }
                    }
                    other => {
                        for &i in &due {
                            other.apply(&mut docs[i].1);
                        }
                    }
                }
            }
            for &i in &due {
                version.apply_defaults(&mut docs[i].1);
            }
        }
        Ok(docs.into_iter().map(|(_, map)| map).collect())
        })
    }

    /// Run a migration function over a batch of documents, as the user who
    /// registered the version. It returns `{"documents": [...]}` (or the array),
    /// one per document given, in order.
    async fn run_migration_function(&self, tess: &str, version: &SchemaVersion, name: &str, docs: Vec<Map<String, Value>>) -> Result<Vec<Map<String, Value>>> {
        let def = self.get_function(name).await?.ok_or_else(|| EngineError::SchemaViolation(format!("migration function '{}' no longer exists", name)))?;
        let (_, user) = crate::users::find_by_id(self, &version.created_by)
            .await?
            .ok_or_else(|| EngineError::SchemaViolation(format!("migration function '{}': the user who registered version {} no longer exists", name, version.version)))?;
        let definitions = crate::users::role_definitions(self).await?;
        let principal = Arc::new(
            crate::auth::Principal::new(version.created_by.clone(), user.login, user.email_address, user.roles, crate::auth::Credential::ApiKey { key_id: format!("migration:{}", name) }, &definitions)
                .with_attributes(user.attributes),
        );
        let count = docs.len();
        let documents = Value::Array(docs.into_iter().map(Value::Object).collect());
        let context = serde_json::json!({ "tessellation": tess, "from_version": version.version - 1, "to_version": version.version, "documents": documents });
        let mut args = Map::new();
        if def.params.iter().any(|p| p.name == "documents") {
            args.insert("documents".into(), documents);
        }
        let run = self.run_function_with(&def, &principal, &args, Some(("migration", context)));
        let output = crate::access::as_caller(principal.clone(), run).await.map_err(|e| EngineError::SchemaViolation(format!("migration function '{}' failed: {:#}", name, e)))?;
        let list = match output {
            Value::Array(list) => list,
            Value::Object(mut map) => match map.remove("documents") {
                Some(Value::Array(list)) => list,
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        if list.len() != count || !list.iter().all(Value::is_object) {
            return Err(EngineError::SchemaViolation(format!(
                "migration function '{}' must return {} documents (as {{\"documents\": [...]}}), in order; it returned {}",
                name,
                count,
                list.len()
            ))
            .into());
        }
        Ok(list
            .into_iter()
            .map(|v| {
                let mut map = match v {
                    Value::Object(map) => map,
                    _ => Map::new(),
                };
                for reserved in crate::document::RESERVED_FIELDS {
                    map.remove(*reserved);
                }
                map
            })
            .collect())
    }

    /// Validate (and upgrade, and fill in defaults for) documents being
    /// written to tessellations with a schema. Called inside the commit.
    pub(crate) async fn apply_schemas(&self, items: &mut [BatchItem]) -> Result<()> {
        let mut cache: std::collections::HashMap<String, Vec<SchemaVersion>> = std::collections::HashMap::new();
        for item in items.iter_mut() {
            let WalOp::Put(doc) = &mut item.op else { continue };
            let versions = cache.entry(doc.tessellation.clone()).or_insert_with(|| self.schemas(&doc.tessellation));
            let Some(current) = versions.last() else { continue };
            let mut map = to_map(&doc.data);
            // A document without a stamp came from the client, in the current
            // shape; a stamped older one is a stored document being patched.
            let from = version_of(&doc.data);
            if doc.data.contains_key(SCHEMA_FIELD) && from < current.version {
                let tess = doc.tessellation.clone();
                map = self.migrate_maps(&tess, versions, vec![(from, map)]).await?.remove(0);
            }
            current.apply_defaults(&mut map);
            let problems = current.validate(&map);
            if !problems.is_empty() {
                let list: Vec<String> = problems.iter().take(10).map(|p| format!("{} {}", p.field, p.message)).collect();
                return Err(EngineError::SchemaViolation(format!(
                    "The document doesn't fit schema version {} of '{}': {}.",
                    current.version,
                    doc.tessellation,
                    list.join("; ")
                ))
                .into());
            }
            doc.data = from_map(&map, current.version);
        }
        Ok(())
    }

    /// Migrate every document of `tess` to the current schema version, in
    /// batches. Safe to run again: documents already current are skipped.
    pub async fn migrate_schema(self: &Arc<Self>, tess: &str) {
        let versions = self.schemas(tess);
        let Some(current) = versions.last().cloned() else { return };
        if !self.is_writable() {
            return;
        }
        {
            let mut jobs = self.schema_jobs.lock().unwrap();
            if jobs.get(tess).is_some_and(|j| j.state == "running" && j.version == current.version) {
                return;
            }
            jobs.insert(tess.to_string(), MigrationStatus { version: current.version, state: "running".into(), started: chrono::Utc::now().timestamp_millis(), ..Default::default() });
        }
        let update = |f: &dyn Fn(&mut MigrationStatus)| {
            if let Some(job) = self.schema_jobs.lock().unwrap().get_mut(tess) {
                f(job);
            }
        };
        let mut after = None;
        loop {
            // A newer version started its own run.
            if self.schemas(tess).last().map(|v| v.version) != Some(current.version) {
                return;
            }
            let page = match self.list_documents(tess, after, MIGRATION_BATCH).await {
                Ok(page) => page,
                Err(e) => {
                    warn!("⚠️ Schema migration of '{}' stopped: {:#}", tess, e);
                    update(&|j| j.state = "failed".into());
                    return;
                }
            };
            let mut items = Vec::new();
            let (mut unchanged, mut failed, mut errors) = (0u64, 0u64, Vec::new());
            let due: Vec<&crate::document::Document> = page.documents.iter().filter(|d| version_of(&d.data) < current.version).collect();
            unchanged += (page.documents.len() - due.len()) as u64;
            let migrated_maps = match self.migrate_maps(tess, &versions, due.iter().map(|d| (version_of(&d.data), to_map(&d.data))).collect()).await {
                Ok(maps) => maps,
                Err(e) => {
                    // A migration function failed for this batch: report it and go on.
                    failed += due.len() as u64;
                    errors.push(serde_json::json!({ "id": due.first().map(|d| d.id.to_string()), "problems": [{ "field": "", "message": format!("{:#}", e) }] }));
                    Vec::new()
                }
            };
            for (doc, map) in due.iter().zip(migrated_maps) {
                let from = version_of(&doc.data);
                let problems = current.validate(&map);
                if !problems.is_empty() {
                    failed += 1;
                    if errors.len() < 20 {
                        errors.push(serde_json::json!({ "id": doc.id.to_string(), "version": from, "problems": problems }));
                    }
                    continue;
                }
                let key = DocKey::new(tess, doc.id);
                let seq = match self.read_latest(&key).await {
                    Ok((_, seq)) => seq,
                    Err(_) => continue,
                };
                let mut migrated = (*doc).clone();
                migrated.data = from_map(&map, current.version);
                items.push(BatchItem { key, op: WalOp::Put(migrated), expected_seq: Some(seq) });
            }
            let count = items.len() as u64;
            // A document changed meanwhile fails its precondition; the next run picks it up.
            let written = if items.is_empty() { Ok(true) } else { self.commit(items).await };
            match written {
                Ok(true) => update(&|j| {
                    j.migrated += count;
                    j.unchanged += unchanged;
                    j.failed += failed;
                    let room = 20usize.saturating_sub(j.errors.len());
                    j.errors.extend(errors.iter().take(room).cloned());
                }),
                Ok(false) => {
                    // Raced with a write; retry this page.
                    continue;
                }
                Err(e) => {
                    warn!("⚠️ Schema migration of '{}' stopped: {:#}", tess, e);
                    update(&|j| j.state = "failed".into());
                    return;
                }
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        let summary = self.schema_migration(tess).unwrap_or_default();
        update(&|j| {
            j.state = if j.failed > 0 { "failed".into() } else { "done".into() };
            j.finished = Some(chrono::Utc::now().timestamp_millis());
        });
        info!(
            "📐 Schema migration of '{}' to version {}: {} migrated, {} already current, {} didn't fit.",
            tess, current.version, summary.migrated, summary.unchanged, summary.failed
        );
    }

    /// At startup on the Overseer: finish migrations of tessellations whose
    /// documents may not all be on the current version yet.
    pub async fn resume_schema_migrations(self: &Arc<Self>) {
        for (name, info) in self.tessellation_details() {
            if !info.schemas.is_empty() && info.kind == "user" {
                let engine = self.clone();
                tokio::spawn(async move { engine.migrate_schema(&name).await });
            }
        }
    }
}
