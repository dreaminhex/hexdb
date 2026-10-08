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

/// Upgrade a document's fields from version `from` to the latest of `versions`.
fn upgrade(map: &mut Map<String, Value>, from: u32, versions: &[SchemaVersion]) {
    for v in versions.iter().filter(|v| v.version > from) {
        v.migrate(map);
    }
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

    /// Register a new schema version (see `crate::schema` for the rules).
    /// The caller starts the migration with [`HexDBEngine::migrate_schema`].
    pub fn add_schema(&self, tess: &str, input: SchemaInput) -> Result<SchemaVersion> {
        self.ensure_writable()?;
        let mut catalog = self.catalog.lock().unwrap();
        let info = catalog.tessellations.get_mut(tess).ok_or_else(|| EngineError::NotFound(format!("Tessellation '{}' not found.", tess)))?;
        if info.kind != "user" {
            return Err(EngineError::Invalid(format!("'{}' is a system tessellation; it can't have a schema.", tess)).into());
        }
        let version = schema::new_version(&info.schemas, input).map_err(|e| EngineError::Conflict(format!("{:#}", e)))?;
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

    /// Validate (and upgrade, and fill in defaults for) documents being
    /// written to tessellations with a schema. Called inside the commit.
    pub(crate) fn apply_schemas(&self, items: &mut [BatchItem]) -> Result<()> {
        let mut cache: std::collections::HashMap<String, Vec<SchemaVersion>> = std::collections::HashMap::new();
        for item in items.iter_mut() {
            let WalOp::Put(doc) = &mut item.op else { continue };
            let versions = cache.entry(doc.tessellation.clone()).or_insert_with(|| self.schemas(&doc.tessellation));
            let Some(current) = versions.last() else { continue };
            let mut map = to_map(&doc.data);
            // A document without a stamp came from the client, in the current
            // shape; a stamped older one is a stored document being patched.
            if doc.data.contains_key(SCHEMA_FIELD) {
                upgrade(&mut map, version_of(&doc.data), versions);
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
            for doc in &page.documents {
                let from = version_of(&doc.data);
                if from >= current.version {
                    unchanged += 1;
                    continue;
                }
                let mut map = to_map(&doc.data);
                upgrade(&mut map, from, &versions);
                current.apply_defaults(&mut map);
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
                let mut migrated = doc.clone();
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
