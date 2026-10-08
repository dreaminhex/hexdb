// HexDB Core Audit Trail
//
// Security-relevant events (sign-ins and failures, sign-outs, password and
// MFA changes, API keys, user and role changes, tessellation and index
// changes, refused requests, maintenance, settings and shutdown) are stored as
// documents in the `_audit` system tessellation, so they survive restarts and
// replicate to every hex like any other data. Each event expires after
// `security.audit_retention_days` (0 keeps events forever).
//
// Only the Overseer writes. A replica forwards its events to the Overseer
// (signed, like sign-out revocations) and keeps nothing itself, so an event
// is lost if the Overseer is unreachable at that moment; it is still in the
// replica's log. Every event is also logged at INFO under `hexdb::audit`.
//
// Plugins can receive the audit trail with `audit = true` in their manifest,
// e.g. to ship it to a SIEM.

use crate::{
    engine::{DocumentQuery, HexDBEngine},
    filter::{Filter, SortKey},
    index::{IndexDef, IndexKind},
};
use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{info, warn};
use ulid::Ulid;

/// System tessellation holding the audit trail.
pub const AUDIT_TESSELLATION: &str = "_audit";

/// How long a replica waits for the Overseer to take an event.
const FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// One audit event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditEvent {
    /// When it happened (epoch milliseconds).
    pub time: i64,
    /// Who did it: a login, or "-" when unknown (e.g. a failed sign-in's login is the target).
    pub actor: String,
    /// What happened, e.g. `auth.login`, `user.update`, `access.denied`.
    pub action: String,
    /// What it happened to: a login, role, tessellation, path...
    pub target: String,
    /// `ok`, `failed` or `denied`.
    pub outcome: String,
    /// The client address, when there was a request.
    #[serde(default)]
    pub client: String,
    /// The hex that recorded it.
    #[serde(default)]
    pub hex: String,
    #[serde(default)]
    pub details: Value,
}

impl AuditEvent {
    pub fn new(actor: &str, action: &str, target: &str) -> Self {
        AuditEvent {
            time: Utc::now().timestamp_millis(),
            actor: if actor.is_empty() { "-".into() } else { actor.to_string() },
            action: action.to_string(),
            target: target.to_string(),
            outcome: "ok".into(),
            client: String::new(),
            hex: String::new(),
            details: json!({}),
        }
    }

    pub fn outcome(mut self, outcome: &str) -> Self {
        self.outcome = outcome.to_string();
        self
    }

    pub fn client(mut self, client: &str) -> Self {
        self.client = client.to_string();
        self
    }

    pub fn details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
}

/// Filters for reading the audit trail.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditQuery {
    pub actor: Option<String>,
    /// An action, or a prefix ending in '.' (e.g. `auth.`).
    pub action: Option<String>,
    pub target: Option<String>,
    pub outcome: Option<String>,
    /// Epoch milliseconds, inclusive.
    pub since: Option<i64>,
    /// Epoch milliseconds, exclusive.
    pub until: Option<i64>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// Index that orders the audit trail by time.
fn time_index() -> IndexDef {
    IndexDef { name: "time".into(), kind: IndexKind::Field, fields: vec!["time".into()], unique: false, analyzer: None }
}

impl HexDBEngine {
    /// Record an event with outcome `ok`.
    pub async fn audit(&self, actor: &str, action: &str, target: &str, details: Value) {
        self.record_audit(AuditEvent::new(actor, action, target).details(details)).await
    }

    /// Record an event: stored on the Overseer, forwarded from a replica.
    /// Never fails the caller; problems are logged.
    pub async fn record_audit(&self, mut event: AuditEvent) {
        event.hex = self.name.clone();
        info!(
            target: "hexdb::audit",
            actor = %event.actor,
            action = %event.action,
            target_name = %event.target,
            outcome = %event.outcome,
            client = %event.client,
            "📜 {} {} {} ({})",
            event.actor,
            event.action,
            event.target,
            event.outcome
        );
        if self.is_writable() {
            if let Err(e) = self.store_audit(&event).await {
                warn!("⚠️ Couldn't store an audit event ({}): {:#}", event.action, e);
            }
        } else {
            // Forwarding is quick on a healthy lattice; don't hold the request long if it isn't.
            let forwarded = match serde_json::to_value(&event) {
                Ok(body) => tokio::time::timeout(FORWARD_TIMEOUT, crate::replication::post_to_overseer(self, "/lattice/audit", &body))
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out"))),
                Err(e) => Err(e.into()),
            };
            if let Err(e) = forwarded {
                warn!("⚠️ Couldn't forward an audit event ({}) to the Overseer: {:#}", event.action, e);
            }
        }
    }

    /// Store an event (the Overseer, or an event forwarded by a replica).
    pub async fn store_audit(&self, event: &AuditEvent) -> Result<()> {
        let days = self.live().audit_retention_days;
        let ttl = (days > 0).then(|| event.time + days as i64 * 86_400_000);
        let first = !self.tessellation_exists(AUDIT_TESSELLATION);
        self.put_system_document(AUDIT_TESSELLATION, Ulid::new(), serde_json::to_value(event)?, ttl).await?;
        if first {
            self.ensure_internal_index(AUDIT_TESSELLATION, time_index()).await?;
        }
        Ok(())
    }

    /// Make sure the audit trail's time index exists (at startup on the Overseer).
    pub(crate) async fn prepare_audit(&self) -> Result<()> {
        if self.is_writable() && self.tessellation_exists(AUDIT_TESSELLATION) {
            self.ensure_internal_index(AUDIT_TESSELLATION, time_index()).await?;
        }
        Ok(())
    }

    /// Audit events matching `query`, newest first, and how many match in all.
    pub async fn audit_events(&self, query: &AuditQuery) -> Result<(Vec<AuditEvent>, usize)> {
        if !self.tessellation_exists(AUDIT_TESSELLATION) {
            return Ok((Vec::new(), 0));
        }
        let mut conditions = serde_json::Map::new();
        if let Some(actor) = query.actor.as_deref().filter(|s| !s.is_empty()) {
            conditions.insert("actor".into(), json!(actor));
        }
        if let Some(action) = query.action.as_deref().filter(|s| !s.is_empty()) {
            if action.ends_with('.') {
                // A prefix: actions are "<area>.<verb>", so match the range of that area.
                let upper = format!("{}\u{10FFFF}", action);
                conditions.insert("action".into(), json!({ "$gte": action, "$lt": upper }));
            } else {
                conditions.insert("action".into(), json!(action));
            }
        }
        if let Some(target) = query.target.as_deref().filter(|s| !s.is_empty()) {
            conditions.insert("target".into(), json!(target));
        }
        if let Some(outcome) = query.outcome.as_deref().filter(|s| !s.is_empty()) {
            conditions.insert("outcome".into(), json!(outcome));
        }
        let mut time = serde_json::Map::new();
        if let Some(since) = query.since {
            time.insert("$gte".into(), json!(since));
        }
        if let Some(until) = query.until {
            time.insert("$lt".into(), json!(until));
        }
        if !time.is_empty() {
            conditions.insert("time".into(), Value::Object(time));
        }
        let page = self
            .query_documents(
                AUDIT_TESSELLATION,
                &DocumentQuery {
                    filter: Filter::parse(&Value::Object(conditions))?,
                    sort: vec![SortKey { field: "time".into(), descending: true }],
                    offset: query.offset.unwrap_or(0),
                    limit: query.limit.unwrap_or(100).clamp(1, 1000),
                    after: None,
                    with_total: true,
                },
            )
            .await?;
        let events = page.documents.iter().filter_map(|d| serde_json::from_value(d.data_json()).ok()).collect();
        Ok((events, page.total.unwrap_or(0)))
    }
}
