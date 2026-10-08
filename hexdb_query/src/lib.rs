//! HexDB GraphQL API.
//!
//! Queries read tessellations, documents (with filters, sorting and paging),
//! users, roles and server status. Mutations write documents and manage
//! tessellations. Documents are dynamic, so their fields are exposed through
//! the `JSON` scalar. Filters use the same language as the REST API (see
//! `hexdb_core::filter`), e.g. `{ views: { $gte: 10 }, tags: "rust" }`.
//!
//! System tessellations (users, roles, idempotency records) are not reachable
//! through the document fields; use `users` and `roles` instead.

use async_graphql::{
    Context, EmptySubscription, ErrorExtensions, InputObject, Json, Object, Schema, SimpleObject, ID,
};
use chrono::{DateTime, Utc};
use hexdb_core::{
    metrics::collect,
    users::{self, RoleGrant, RoleView, UserView},
    validate_tessellation_name, Document, DocumentQuery, EngineError, Filter, HexDBEngine, IdempotencyKey, SortKey,
    Permission, Principal, TessellationInfo,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::error;
use ulid::Ulid;

/// The HexDB GraphQL schema.
pub type HexDBSchema = Schema<QueryRoot, MutationRoot, EmptySubscription>;

/// Most documents returned by one `documents` field.
pub const MAX_PAGE_SIZE: usize = 1000;

/// Build the schema for an engine.
pub fn build_schema(engine: Arc<HexDBEngine>) -> HexDBSchema {
    Schema::build(QueryRoot, MutationRoot, EmptySubscription)
        .data(engine)
        .limit_depth(16)
        .limit_complexity(10_000)
        .finish()
}

// ---------------------------------------------------------------------------
// Errors and helpers
// ---------------------------------------------------------------------------

type GqlResult<T> = async_graphql::Result<T>;

fn gql_error(code: &'static str, message: impl Into<String>) -> async_graphql::Error {
    async_graphql::Error::new(message.into()).extend_with(|_, ext| ext.set("code", code))
}

/// Convert an engine error into a GraphQL error with an `extensions.code`
/// matching the REST error codes.
fn to_gql(e: anyhow::Error) -> async_graphql::Error {
    match e.downcast_ref::<EngineError>() {
        Some(EngineError::NotFound(m)) => gql_error("NOT_FOUND", m.clone()),
        Some(EngineError::Invalid(m)) => gql_error("INVALID_REQUEST", m.clone()),
        Some(EngineError::Conflict(m)) => gql_error("CONFLICT", m.clone()),
        Some(EngineError::Unprocessable(m)) => gql_error("UNPROCESSABLE", m.clone()),
        Some(EngineError::ReadOnly(m)) => gql_error("READ_ONLY_REPLICA", m.clone()),
        Some(EngineError::Unauthorized(m)) | Some(EngineError::MfaRequired(m)) => gql_error("UNAUTHENTICATED", m.clone()),
        Some(EngineError::ReplicationTimeout(m)) => gql_error("REPLICATION_TIMEOUT", m.clone()),
        Some(EngineError::NoQuorum(m)) => gql_error("NO_QUORUM", m.clone()),
        Some(EngineError::TooLarge(m)) => gql_error("DOCUMENT_TOO_LARGE", m.clone()),
        Some(EngineError::SchemaViolation(m)) => gql_error("SCHEMA_VIOLATION", m.clone()),
        Some(EngineError::TriggerRejected(m)) => gql_error("TRIGGER_REJECTED", m.clone()),
        Some(EngineError::DiskFull(m)) => gql_error("DISK_FULL", m.clone()),
        Some(EngineError::Forbidden(m)) => gql_error("FORBIDDEN", m.clone()),
        Some(EngineError::RateLimited(m, _)) => gql_error("RATE_LIMITED", m.clone()),
        None => {
            error!("❌ GraphQL request failed: {:#}", e);
            gql_error("INTERNAL", "The request failed; see the server log.")
        }
    }
}

fn engine<'a>(ctx: &Context<'a>) -> &'a Arc<HexDBEngine> {
    ctx.data_unchecked::<Arc<HexDBEngine>>()
}

/// The authenticated caller. The HTTP handler attaches it to every request;
/// a request without one is refused.
fn principal<'a>(ctx: &Context<'a>) -> GqlResult<&'a Principal> {
    ctx.data_opt::<Principal>()
        .ok_or_else(|| gql_error("UNAUTHENTICATED", "Sign in, or send an API key."))
}

fn require(ctx: &Context<'_>, permission: Permission, tessellation: &str) -> GqlResult<()> {
    principal(ctx)?.require(permission, tessellation).map_err(to_gql)
}

fn require_admin(ctx: &Context<'_>) -> GqlResult<()> {
    principal(ctx)?.require_admin().map_err(to_gql)
}

fn require_action(ctx: &Context<'_>, action: hexdb_core::Action) -> GqlResult<()> {
    principal(ctx)?.require_action(action).map_err(to_gql)
}

/// Validate a tessellation name for document access, refusing system tessellations.
fn user_tessellation(engine: &HexDBEngine, tess: &str) -> GqlResult<()> {
    validate_tessellation_name(tess).map_err(|e| gql_error("INVALID_REQUEST", e.to_string()))?;
    if engine.is_system_tessellation(tess) {
        return Err(gql_error(
            "FORBIDDEN",
            format!("'{}' is a system tessellation; use the users or roles fields.", tess),
        ));
    }
    Ok(())
}

fn existing_tessellation(engine: &HexDBEngine, tess: &str) -> GqlResult<()> {
    user_tessellation(engine, tess)?;
    if !engine.tessellation_exists(tess) {
        return Err(gql_error("NOT_FOUND", format!("Tessellation '{}' not found.", tess)));
    }
    Ok(())
}

fn millis_to_rfc3339(millis: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp_millis(millis).map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn expiry(ttl: Option<u64>) -> Option<i64> {
    ttl.map(|secs| {
        let millis = secs.min(i64::MAX as u64 / 1000) as i64 * 1000;
        Utc::now().timestamp_millis().saturating_add(millis)
    })
}

fn parse_filter(filter: Option<Json<Value>>) -> GqlResult<Filter> {
    match filter {
        Some(Json(value)) => Filter::parse(&value).map_err(to_gql),
        None => Ok(Filter::all()),
    }
}

fn parse_after(after: Option<ID>) -> GqlResult<Option<Ulid>> {
    after
        .map(|id| Ulid::from_string(&id).map_err(|_| gql_error("INVALID_REQUEST", "after must be a document ID.")))
        .transpose()
}

async fn query_page(
    ctx: &Context<'_>,
    tess: &str,
    filter: Option<Json<Value>>,
    sort: Option<Vec<SortInput>>,
    limit: Option<i32>,
    offset: Option<i32>,
    after: Option<ID>,
) -> GqlResult<DocumentPage> {
    let engine = engine(ctx);
    existing_tessellation(engine, tess)?;
    let limit = limit.unwrap_or(100);
    if limit < 0 || limit as usize > MAX_PAGE_SIZE {
        return Err(gql_error("INVALID_REQUEST", format!("limit must be 0-{}.", MAX_PAGE_SIZE)));
    }
    // Count every match only if the client asked for `total`; otherwise a
    // sorted or filtered query can stop once its page is full.
    let with_total = ctx.look_ahead().field("total").exists();
    let query = DocumentQuery {
        filter: parse_filter(filter)?,
        sort: sort.unwrap_or_default().into_iter().map(Into::into).collect(),
        offset: offset.unwrap_or(0).max(0) as usize,
        limit: limit as usize,
        after: parse_after(after)?,
        with_total,
    };
    let page = engine.query_documents(tess, &query).await.map_err(to_gql)?;
    Ok(DocumentPage {
        documents: page.documents.into_iter().map(DocumentObject).collect(),
        total: page.total.unwrap_or(0),
        next: page.next.map(|id| ID(id.to_string())),
        indexes_used: page.indexes,
        scanned: page.scanned,
    })
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A sort key: a field path, ascending unless `descending` is set.
#[derive(InputObject, Clone)]
pub struct SortInput {
    pub field: String,
    #[graphql(default)]
    pub descending: bool,
}

impl From<SortInput> for SortKey {
    fn from(s: SortInput) -> Self {
        SortKey { field: s.field, descending: s.descending }
    }
}

/// A document.
pub struct DocumentObject(Document);

#[Object(name = "Document")]
impl DocumentObject {
    async fn id(&self) -> ID {
        ID(self.0.id.to_string())
    }

    async fn tessellation(&self) -> &str {
        &self.0.tessellation
    }

    /// When the document expires (RFC 3339), if it has a TTL.
    async fn expires_at(&self) -> Option<String> {
        self.0.ttl.and_then(millis_to_rfc3339)
    }

    /// The document's fields.
    async fn data(&self, #[graphql(desc = "Only these dotted field paths.")] fields: Option<Vec<String>>) -> Json<Value> {
        if let Some(fields) = fields {
            let mut projected = hexdb_core::project(&self.0.to_api_json(), &fields);
            if let Value::Object(map) = &mut projected {
                map.remove("id");
                map.remove("_expires_at");
            }
            return Json(projected);
        }
        Json(self.0.data_json())
    }

    /// The document as returned by the REST API: fields plus `id` and `_expires_at`.
    async fn json(&self) -> Json<Value> {
        Json(self.0.to_api_json())
    }

    /// One field by dotted path (e.g. "author.name"), or null if missing.
    async fn field(&self, path: String) -> Option<Json<Value>> {
        let mut current = self.0.data_json();
        for part in path.split('.') {
            current = current.get(part)?.clone();
        }
        Some(Json(current))
    }
}

/// One page of documents.
#[derive(SimpleObject)]
pub struct DocumentPage {
    pub documents: Vec<DocumentObject>,
    /// Documents matching the filter, across all pages (counted only when selected).
    pub total: usize,
    /// Pass as `after` for the next page (unsorted queries only).
    pub next: Option<ID>,
    /// Indexes the query used (empty: every document was scanned).
    pub indexes_used: Vec<String>,
    /// Documents read to answer the query.
    pub scanned: usize,
}

/// A secondary index.
#[derive(SimpleObject)]
#[graphql(name = "Index")]
pub struct IndexObject {
    pub name: String,
    /// "field" or "text".
    pub kind: String,
    pub fields: Vec<String>,
    pub unique: bool,
    /// Text indexes: the analyzer ("standard" unless set).
    pub analyzer: Option<String>,
    /// Documents indexed.
    pub documents: usize,
    /// Distinct keys (field index) or words (text index).
    pub keys: usize,
    pub ready: bool,
}

impl From<hexdb_core::IndexInfo> for IndexObject {
    fn from(info: hexdb_core::IndexInfo) -> Self {
        IndexObject {
            name: info.def.name,
            kind: match info.def.kind {
                hexdb_core::IndexKind::Field => "field".into(),
                hexdb_core::IndexKind::Text => "text".into(),
            },
            fields: info.def.fields,
            unique: info.def.unique,
            analyzer: (info.def.kind == hexdb_core::IndexKind::Text).then(|| info.def.analyzer.clone().unwrap_or_else(|| "standard".into())),
            documents: info.documents,
            keys: info.keys,
            ready: info.ready,
        }
    }
}

/// A tessellation (collection of documents).
pub struct TessellationObject {
    name: String,
    info: TessellationInfo,
}

#[Object(name = "Tessellation")]
impl TessellationObject {
    async fn name(&self) -> &str {
        &self.name
    }

    /// "user" or "system".
    async fn kind(&self) -> &str {
        &self.info.kind
    }

    /// Creation time (RFC 3339).
    async fn created(&self) -> Option<String> {
        millis_to_rfc3339(self.info.created)
    }

    /// Secondary indexes.
    async fn indexes(&self, ctx: &Context<'_>) -> GqlResult<Vec<IndexObject>> {
        require(ctx, Permission::Read, &self.name)?;
        Ok(engine(ctx).list_indexes(&self.name).into_iter().map(Into::into).collect())
    }

    /// Number of documents, optionally only those matching a filter.
    async fn document_count(&self, ctx: &Context<'_>, filter: Option<Json<Value>>) -> GqlResult<usize> {
        let engine = engine(ctx);
        if engine.is_system_tessellation(&self.name) {
            require_action(ctx, hexdb_core::Action::Status)?;
        } else {
            require(ctx, Permission::Read, &self.name)?;
        }
        if engine.is_system_tessellation(&self.name) && filter.is_some() {
            return Err(gql_error("FORBIDDEN", "System tessellations can't be filtered."));
        }
        engine.count_matching(&self.name, &parse_filter(filter)?).await.map_err(to_gql)
    }

    /// Documents in this tessellation (user tessellations only).
    async fn documents(
        &self,
        ctx: &Context<'_>,
        filter: Option<Json<Value>>,
        sort: Option<Vec<SortInput>>,
        limit: Option<i32>,
        offset: Option<i32>,
        after: Option<ID>,
    ) -> GqlResult<DocumentPage> {
        require(ctx, Permission::Read, &self.name)?;
        query_page(ctx, &self.name, filter, sort, limit, offset, after).await
    }
}

/// Aggregation results.
#[derive(SimpleObject)]
#[graphql(name = "AggregateResult")]
pub struct AggregateResultObject {
    /// One object per group: the groupBy fields plus each aggregate.
    pub rows: Vec<Json<Value>>,
    /// Groups before limit/offset.
    pub total_groups: usize,
    /// Documents that matched the filter.
    pub matched: usize,
}

/// The outcome of one transaction operation.
#[derive(SimpleObject)]
#[graphql(name = "TransactionOperationResult")]
pub struct TxResultObject {
    pub op: String,
    pub tessellation: String,
    pub id: ID,
    /// The document's version after the commit (null once deleted).
    pub version: Option<u64>,
    /// The document as of this operation (get, insert, replace, patch).
    pub document: Option<Json<Value>>,
}

/// A committed transaction.
#[derive(SimpleObject)]
#[graphql(name = "TransactionResult")]
pub struct TransactionResultObject {
    pub results: Vec<TxResultObject>,
    /// Documents written.
    pub writes: usize,
}

/// A page of the change feed.
#[derive(SimpleObject)]
pub struct ChangePage {
    /// `{seq, timestamp, op: put|delete|drop_tessellation, tessellation, id, document}`.
    pub changes: Vec<Json<Value>>,
    /// Pass as `after` for the next page.
    pub last_seq: u64,
}

/// Result of `updateDocuments`.
#[derive(SimpleObject)]
pub struct UpdateResult {
    pub matched: usize,
    pub modified: usize,
}

/// A role granted to a user, on these tessellations ("*" for all).
#[derive(SimpleObject)]
pub struct RoleGrantObject {
    pub name: String,
    pub tessellations: Vec<String>,
}

impl From<RoleGrant> for RoleGrantObject {
    fn from(g: RoleGrant) -> Self {
        RoleGrantObject { name: g.name, tessellations: g.tessellations }
    }
}

/// A user. Password hashes and MFA secrets are never exposed.
#[derive(SimpleObject)]
#[graphql(name = "User")]
pub struct UserObject {
    pub id: ID,
    pub login: String,
    pub email_address: String,
    pub roles: Vec<RoleGrantObject>,
    pub created: i64,
    pub last_login: i64,
    pub is_locked: bool,
    pub use_mfa: bool,
}

impl From<UserView> for UserObject {
    fn from(u: UserView) -> Self {
        UserObject {
            id: ID(u.id),
            login: u.login,
            email_address: u.email_address,
            roles: u.roles.into_iter().map(Into::into).collect(),
            created: u.created,
            last_login: u.last_login,
            is_locked: u.is_locked,
            use_mfa: u.use_mfa,
        }
    }
}

/// A role.
#[derive(SimpleObject)]
#[graphql(name = "Role")]
pub struct RoleObject {
    pub id: ID,
    pub name: String,
    pub description: String,
    /// The permissions the role holds (read, write, manage, status, logs, audit, plugins, maintenance, admin).
    pub permissions: Vec<String>,
    /// Built-in roles can't be changed.
    pub builtin: bool,
}

impl From<RoleView> for RoleObject {
    fn from(r: RoleView) -> Self {
        RoleObject {
            id: ID(r.id),
            name: r.name,
            description: r.description,
            permissions: r.permissions.iter().map(|a| a.name().to_string()).collect(),
            builtin: r.builtin,
        }
    }
}

// ---------------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct QueryRoot;

#[Object]
#[allow(clippy::too_many_arguments)]
impl QueryRoot {
    /// Tessellations you can read (administrators see all but internal ones).
    async fn tessellations(&self, ctx: &Context<'_>) -> GqlResult<Vec<TessellationObject>> {
        let principal = principal(ctx)?;
        Ok(engine(ctx)
            .tessellation_details()
            .into_iter()
            .filter(|(name, info)| {
                if principal.is_admin() {
                    !name.starts_with('_')
                } else {
                    info.kind == "user" && principal.can(Permission::Read, name)
                }
            })
            .map(|(name, info)| TessellationObject { name, info })
            .collect())
    }

    /// One tessellation by name.
    async fn tessellation(&self, ctx: &Context<'_>, name: String) -> GqlResult<Option<TessellationObject>> {
        let engine = engine(ctx);
        if engine.is_system_tessellation(&name) {
            require_action(ctx, hexdb_core::Action::Status)?;
        } else {
            require(ctx, Permission::Read, &name)?;
        }
        Ok(engine.tessellation_info(&name).map(|info| TessellationObject { name, info }))
    }

    /// One document by ID.
    async fn document(&self, ctx: &Context<'_>, tessellation: String, id: ID) -> GqlResult<Option<DocumentObject>> {
        require(ctx, Permission::Read, &tessellation)?;
        let engine = engine(ctx);
        user_tessellation(engine, &tessellation)?;
        Ok(engine.get_document(&tessellation, &id).await.map_err(to_gql)?.map(DocumentObject))
    }

    /// Documents matching a filter. Without `sort`, results are in ID order and
    /// `next` pages forward via `after`; with `sort`, page with `offset`.
    async fn documents(
        &self,
        ctx: &Context<'_>,
        tessellation: String,
        filter: Option<Json<Value>>,
        sort: Option<Vec<SortInput>>,
        #[graphql(desc = "Page size, 0-1000 (default 100).")] limit: Option<i32>,
        offset: Option<i32>,
        after: Option<ID>,
    ) -> GqlResult<DocumentPage> {
        require(ctx, Permission::Read, &tessellation)?;
        query_page(ctx, &tessellation, filter, sort, limit, offset, after).await
    }

    /// Number of documents, optionally only those matching a filter.
    async fn count(&self, ctx: &Context<'_>, tessellation: String, filter: Option<Json<Value>>) -> GqlResult<usize> {
        require(ctx, Permission::Read, &tessellation)?;
        let engine = engine(ctx);
        existing_tessellation(engine, &tessellation)?;
        engine.count_matching(&tessellation, &parse_filter(filter)?).await.map_err(to_gql)
    }

    /// Group matching documents and summarize each group. `aggregates` maps
    /// output names to one operator each, e.g.
    /// `{total: {_sum: "price"}, orders: {_count: "*"}, avg: {_avg: "price"}}`.
    /// Operators: count (`"*"` or a field), countDistinct, sum, avg, min, max.
    /// Each row holds the groupBy fields (by dotted name) and the aggregates.
    async fn aggregate(
        &self,
        ctx: &Context<'_>,
        tessellation: String,
        filter: Option<Json<Value>>,
        #[graphql(desc = "Field paths to group by; omit for one row over every match.")] group_by: Option<Vec<String>>,
        #[graphql(desc = "Defaults to {count: {_count: \"*\"}}.")] aggregates: Option<Json<Value>>,
        #[graphql(desc = "Sort by groupBy fields or aggregate names. Defaults to groupBy fields ascending.")] sort: Option<Vec<SortInput>>,
        #[graphql(desc = "Rows to return, 0-10000 (default 1000).")] limit: Option<i32>,
        offset: Option<i32>,
    ) -> GqlResult<AggregateResultObject> {
        require(ctx, Permission::Read, &tessellation)?;
        let engine = engine(ctx);
        existing_tessellation(engine, &tessellation)?;
        let limit = limit.unwrap_or(1000);
        if !(0..=10_000).contains(&limit) {
            return Err(gql_error("INVALID_REQUEST", "limit must be 0-10000."));
        }
        let aggregates = hexdb_core::parse_aggregates(&aggregates.map(|j| j.0).unwrap_or(Value::Null)).map_err(to_gql)?;
        let aggregation = hexdb_core::Aggregation::new(
            parse_filter(filter)?,
            group_by.unwrap_or_default(),
            aggregates,
            sort.unwrap_or_default().into_iter().map(Into::into).collect(),
            offset.unwrap_or(0).max(0) as usize,
            limit as usize,
        )
        .map_err(to_gql)?;
        let result = engine.aggregate(&tessellation, &aggregation).await.map_err(to_gql)?;
        Ok(AggregateResultObject {
            rows: result.rows.into_iter().map(|row| Json(Value::Object(row))).collect(),
            total_groups: result.total_groups,
            matched: result.matched,
        })
    }

    /// Committed changes after a sequence number, oldest first (system
    /// tessellations are left out). Pass `lastSeq` back as `after` to continue;
    /// omit `after` to start from now. For a live feed use
    /// `GET /changes/stream` (Server-Sent Events).
    async fn changes(
        &self,
        ctx: &Context<'_>,
        after: Option<u64>,
        tessellation: Option<String>,
        #[graphql(desc = "Changes to return, 1-1000 (default 100).")] limit: Option<i32>,
    ) -> GqlResult<ChangePage> {
        let principal = principal(ctx)?;
        if let Some(t) = tessellation.as_deref() {
            require(ctx, Permission::Read, t)?;
        }
        let engine = engine(ctx);
        let limit = limit.unwrap_or(100).clamp(1, 1000) as usize;
        let mut cursor = after.unwrap_or_else(|| engine.changes.published_seq());
        let backlog = engine.changes.since(cursor, usize::MAX).map_err(|e| {
            gql_error(
                "HISTORY_EXPIRED",
                format!("Changes before sequence {} are no longer kept.", e.available_after + 1),
            )
        })?;
        let mut changes = Vec::new();
        for change in backlog {
            if changes.len() == limit {
                break;
            }
            cursor = change.seq;
            let system = if change.kind == hexdb_core::ChangeKind::DropTessellation {
                change.tessellation.starts_with('_')
            } else {
                engine.is_system_tessellation(&change.tessellation)
            };
            if !system
                && principal.can(Permission::Read, &change.tessellation)
                && tessellation.as_deref().is_none_or(|t| t == change.tessellation)
            {
                changes.push(Json(change.to_api_json()));
            }
        }
        Ok(ChangePage { changes, last_seq: cursor })
    }

    /// All users.
    async fn users(&self, ctx: &Context<'_>) -> GqlResult<Vec<UserObject>> {
        require_admin(ctx)?;
        Ok(users::list_users(engine(ctx)).await.map_err(to_gql)?.into_iter().map(Into::into).collect())
    }

    /// One user by ID or login.
    async fn user(&self, ctx: &Context<'_>, id_or_login: String) -> GqlResult<Option<UserObject>> {
        require_admin(ctx)?;
        Ok(users::get_user(engine(ctx), &id_or_login).await.map_err(to_gql)?.map(Into::into))
    }

    /// All roles.
    async fn roles(&self, ctx: &Context<'_>) -> GqlResult<Vec<RoleObject>> {
        require_admin(ctx)?;
        Ok(users::list_roles(engine(ctx)).await.map_err(to_gql)?.into_iter().map(Into::into).collect())
    }

    /// Server status and metrics (the same data as GET /status).
    async fn status(&self, ctx: &Context<'_>) -> GqlResult<Json<Value>> {
        require_action(ctx, hexdb_core::Action::Status)?;
        let meta = collect(engine(ctx)).await;
        Ok(Json(serde_json::to_value(meta).map_err(|e| gql_error("INTERNAL", e.to_string()))?))
    }
}

// ---------------------------------------------------------------------------
// Mutation
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct MutationRoot;

#[Object]
#[allow(clippy::too_many_arguments)]
impl MutationRoot {
    /// Insert a document. `ttl` is in seconds.
    async fn insert_document(
        &self,
        ctx: &Context<'_>,
        tessellation: String,
        data: Json<Value>,
        ttl: Option<u64>,
        #[graphql(desc = "Retrying with the same key and arguments returns the original result instead of writing again (kept 24 hours). Reusing a key with different arguments fails with UNPROCESSABLE.")]
        idempotency_key: Option<String>,
    ) -> GqlResult<DocumentObject> {
        require(ctx, Permission::Write, &tessellation)?;
        let engine = engine(ctx);
        user_tessellation(engine, &tessellation)?;
        let idem = idempotency(
            ctx,
            "insertDocument",
            idempotency_key,
            json!({ "tessellation": tessellation, "data": data.0, "ttl": ttl }),
        )?;
        let outcome = engine
            .insert_documents(&tessellation, vec![data.0], expiry(ttl), idem)
            .await
            .map_err(to_gql)?;
        note_replay(ctx, outcome.replayed);
        let mut docs = outcome.value;
        Ok(DocumentObject(docs.remove(0)))
    }

    /// Insert many documents atomically.
    async fn insert_documents(
        &self,
        ctx: &Context<'_>,
        tessellation: String,
        documents: Vec<Json<Value>>,
        ttl: Option<u64>,
        #[graphql(desc = "Retrying with the same key and arguments returns the original result instead of writing again (kept 24 hours). Reusing a key with different arguments fails with UNPROCESSABLE.")]
        idempotency_key: Option<String>,
    ) -> GqlResult<Vec<DocumentObject>> {
        require(ctx, Permission::Write, &tessellation)?;
        let engine = engine(ctx);
        user_tessellation(engine, &tessellation)?;
        let docs: Vec<Value> = documents.into_iter().map(|d| d.0).collect();
        let idem = idempotency(
            ctx,
            "insertDocuments",
            idempotency_key,
            json!({ "tessellation": tessellation, "documents": docs, "ttl": ttl }),
        )?;
        let outcome = engine.insert_documents(&tessellation, docs, expiry(ttl), idem).await.map_err(to_gql)?;
        note_replay(ctx, outcome.replayed);
        Ok(outcome.value.into_iter().map(DocumentObject).collect())
    }

    /// Replace a document's fields.
    async fn replace_document(
        &self,
        ctx: &Context<'_>,
        tessellation: String,
        id: ID,
        data: Json<Value>,
        ttl: Option<u64>,
        #[graphql(desc = "Retrying with the same key and arguments returns the original result instead of writing again (kept 24 hours). Reusing a key with different arguments fails with UNPROCESSABLE.")]
        idempotency_key: Option<String>,
    ) -> GqlResult<DocumentObject> {
        require(ctx, Permission::Write, &tessellation)?;
        let engine = engine(ctx);
        user_tessellation(engine, &tessellation)?;
        let idem = idempotency(
            ctx,
            "replaceDocument",
            idempotency_key,
            json!({ "tessellation": tessellation, "id": id.as_str(), "data": data.0, "ttl": ttl }),
        )?;
        let outcome = engine
            .replace_document(&tessellation, &id, data.0, expiry(ttl), idem)
            .await
            .map_err(to_gql)?;
        note_replay(ctx, outcome.replayed);
        Ok(DocumentObject(outcome.value))
    }

    /// Merge fields into a document (a null field removes it).
    async fn patch_document(
        &self,
        ctx: &Context<'_>,
        tessellation: String,
        id: ID,
        data: Json<Value>,
        ttl: Option<u64>,
        #[graphql(desc = "Retrying with the same key and arguments returns the original result instead of writing again (kept 24 hours). Reusing a key with different arguments fails with UNPROCESSABLE.")]
        idempotency_key: Option<String>,
    ) -> GqlResult<DocumentObject> {
        require(ctx, Permission::Write, &tessellation)?;
        let engine = engine(ctx);
        user_tessellation(engine, &tessellation)?;
        let idem = idempotency(
            ctx,
            "patchDocument",
            idempotency_key,
            json!({ "tessellation": tessellation, "id": id.as_str(), "data": data.0, "ttl": ttl }),
        )?;
        let outcome = engine
            .patch_document(&tessellation, &id, data.0, expiry(ttl), idem)
            .await
            .map_err(to_gql)?;
        note_replay(ctx, outcome.replayed);
        Ok(DocumentObject(outcome.value))
    }

    /// Delete a document. Returns false if it didn't exist.
    async fn delete_document(
        &self,
        ctx: &Context<'_>,
        tessellation: String,
        id: ID,
        #[graphql(desc = "Retrying with the same key and arguments returns the original result instead of writing again (kept 24 hours). Reusing a key with different arguments fails with UNPROCESSABLE.")]
        idempotency_key: Option<String>,
    ) -> GqlResult<bool> {
        require(ctx, Permission::Write, &tessellation)?;
        let engine = engine(ctx);
        user_tessellation(engine, &tessellation)?;
        let idem = idempotency(
            ctx,
            "deleteDocument",
            idempotency_key,
            json!({ "tessellation": tessellation, "id": id.as_str() }),
        )?;
        let outcome = engine.delete_document(&tessellation, &id, idem).await.map_err(to_gql)?;
        note_replay(ctx, outcome.replayed);
        Ok(outcome.value)
    }

    /// Run operations across tessellations atomically: all succeed or nothing
    /// is written. `operations` is a list like
    /// `[{op: "patch", tessellation: "accounts", id: "...", data: {balance: 70}, if_match: {balance: {_gte: 30}}},
    ///   {op: "insert", tessellation: "ledger", data: {amount: -30}}]`.
    /// Ops: get, check, insert, replace, patch, delete. Preconditions:
    /// `if_version` (0 = must not exist) and `if_match` (a filter). A failed
    /// precondition fails with CONFLICT.
    async fn transaction(
        &self,
        ctx: &Context<'_>,
        operations: Json<Value>,
        #[graphql(desc = "Retrying with the same key and arguments returns the original result instead of writing again (kept 24 hours). Reusing a key with different arguments fails with UNPROCESSABLE.")]
        idempotency_key: Option<String>,
    ) -> GqlResult<TransactionResultObject> {
        let engine = engine(ctx);
        let ops = hexdb_core::parse_transaction(&operations.0).map_err(to_gql)?;
        for op in &ops {
            let needed = match op.kind {
                hexdb_core::TxOpKind::Get | hexdb_core::TxOpKind::Check => Permission::Read,
                _ => Permission::Write,
            };
            require(ctx, needed, &op.tessellation)?;
        }
        let idem = idempotency(ctx, "transaction", idempotency_key, json!({ "operations": operations.0 }))?;
        let outcome = engine.transaction(&ops, idem).await.map_err(to_gql)?;
        note_replay(ctx, outcome.replayed);
        Ok(TransactionResultObject {
            writes: outcome.value.writes,
            results: outcome
                .value
                .results
                .into_iter()
                .map(|r| TxResultObject {
                    op: format!("{:?}", r.op).to_lowercase(),
                    tessellation: r.tessellation,
                    id: ID(r.id),
                    version: r.version,
                    document: r.document.map(Json),
                })
                .collect(),
        })
    }

    /// Merge-patch every document matching a filter, atomically.
    async fn update_documents(
        &self,
        ctx: &Context<'_>,
        tessellation: String,
        filter: Json<Value>,
        update: Json<Value>,
        ttl: Option<u64>,
        #[graphql(desc = "Retrying with the same key and arguments returns the original result instead of writing again (kept 24 hours). Reusing a key with different arguments fails with UNPROCESSABLE.")]
        idempotency_key: Option<String>,
    ) -> GqlResult<UpdateResult> {
        require(ctx, Permission::Write, &tessellation)?;
        let engine = engine(ctx);
        existing_tessellation(engine, &tessellation)?;
        let idem = idempotency(
            ctx,
            "updateDocuments",
            idempotency_key,
            json!({ "tessellation": tessellation, "filter": filter.0, "update": update.0, "ttl": ttl }),
        )?;
        let outcome = engine
            .update_where(&tessellation, &filter.0, &update.0, expiry(ttl), idem)
            .await
            .map_err(to_gql)?;
        note_replay(ctx, outcome.replayed);
        Ok(UpdateResult { matched: outcome.value.matched, modified: outcome.value.modified })
    }

    /// Create a user tessellation.
    async fn create_tessellation(&self, ctx: &Context<'_>, name: String) -> GqlResult<TessellationObject> {
        require(ctx, Permission::Write, &name)?;
        let engine = engine(ctx);
        user_tessellation(engine, &name)?;
        if !engine.create_tessellation(&name, "user").map_err(to_gql)? {
            return Err(gql_error("CONFLICT", format!("Tessellation '{}' already exists.", name)));
        }
        let info = engine
            .tessellation_info(&name)
            .ok_or_else(|| gql_error("INTERNAL", "Tessellation vanished."))?;
        engine.audit(&principal(ctx)?.login, "tessellation.create", &name, serde_json::json!({})).await;
        Ok(TessellationObject { name, info })
    }

    /// Create an index and build it from the existing documents. A field index
    /// speeds up equality, $in, range, and $startsWith filters on its first
    /// field (and equality on all its fields); a text index speeds up $text.
    async fn create_index(
        &self,
        ctx: &Context<'_>,
        tessellation: String,
        fields: Vec<String>,
        #[graphql(desc = "Defaults to the field names joined with _.")] name: Option<String>,
        #[graphql(desc = "\"field\" (default) or \"text\".")] kind: Option<String>,
        #[graphql(default)] unique: bool,
        #[graphql(desc = "Text indexes: the analyzer (standard, simple, whitespace, keyword, english, ngram, autocomplete, or a custom one).")] analyzer: Option<String>,
    ) -> GqlResult<IndexObject> {
        require(ctx, Permission::Manage, &tessellation)?;
        let engine = engine(ctx);
        existing_tessellation(engine, &tessellation)?;
        let kind = match kind.as_deref().unwrap_or("field") {
            "field" => hexdb_core::IndexKind::Field,
            "text" => hexdb_core::IndexKind::Text,
            other => return Err(gql_error("INVALID_REQUEST", format!("Unknown index kind '{}'; use field or text.", other))),
        };
        let def = hexdb_core::IndexDef { name: name.unwrap_or_default(), kind, fields, unique, analyzer };
        let info = engine.create_index(&tessellation, def).await.map_err(to_gql)?;
        engine
            .audit(&principal(ctx)?.login, "index.create", &tessellation, serde_json::json!({ "index": info.def.name, "fields": info.def.fields }))
            .await;
        Ok(info.into())
    }

    /// Drop an index. Returns false if it didn't exist.
    async fn drop_index(&self, ctx: &Context<'_>, tessellation: String, name: String) -> GqlResult<bool> {
        require(ctx, Permission::Manage, &tessellation)?;
        let engine = engine(ctx);
        existing_tessellation(engine, &tessellation)?;
        let dropped = engine.drop_index(&tessellation, &name).map_err(to_gql)?;
        if dropped {
            engine.audit(&principal(ctx)?.login, "index.drop", &tessellation, serde_json::json!({ "index": name })).await;
        }
        Ok(dropped)
    }

    /// Delete a user tessellation and all of its documents. Returns false if it didn't exist.
    async fn delete_tessellation(&self, ctx: &Context<'_>, name: String) -> GqlResult<bool> {
        require(ctx, Permission::Manage, &name)?;
        let engine = engine(ctx);
        if engine.tessellation_exists(&name) && engine.is_system_tessellation(&name) {
            return Err(gql_error("FORBIDDEN", format!("'{}' is a system tessellation and can't be deleted.", name)));
        }
        let deleted = engine.delete_tessellation(&name).await.map_err(to_gql)?;
        if deleted {
            engine.audit(&principal(ctx)?.login, "tessellation.delete", &name, serde_json::json!({})).await;
        }
        Ok(deleted)
    }
}

// ---------------------------------------------------------------------------
// Idempotency
// ---------------------------------------------------------------------------

/// Records which mutations in one request were answered from an idempotency record.
#[derive(Default)]
pub struct ReplayLog(std::sync::Mutex<Vec<String>>);

/// Execute a request. Mutations answered from an idempotency record are listed
/// (by response path) in `extensions.idempotentReplays` and returned.
pub async fn execute(schema: &HexDBSchema, request: async_graphql::Request) -> (async_graphql::Response, Vec<String>) {
    let log = Arc::new(ReplayLog::default());
    let mut response = schema.execute(request.data(log.clone())).await;
    let replays = log.0.lock().unwrap().clone();
    if !replays.is_empty() {
        response.extensions.insert(
            "idempotentReplays".into(),
            async_graphql::Value::List(replays.iter().cloned().map(async_graphql::Value::String).collect()),
        );
    }
    (response, replays)
}

/// Build an idempotency key from a mutation's name and canonical arguments.
fn idempotency(ctx: &Context<'_>, mutation: &str, key: Option<String>, args: Value) -> GqlResult<Option<IdempotencyKey>> {
    let Some(key) = key else { return Ok(None) };
    // serde_json orders object keys, so this is canonical.
    let request = format!("graphql {}\n{}", mutation, serde_json::to_string(&args).unwrap_or_default());
    // Keys are per user, so users can't replay or block each other's.
    let owner = principal(ctx)?.user_id.clone();
    IdempotencyKey::new(&key, request.as_bytes()).map(|k| Some(k.scoped_to(&owner))).map_err(to_gql)
}

fn note_replay(ctx: &Context<'_>, replayed: bool) {
    if !replayed {
        return;
    }
    if let Some(log) = ctx.data_opt::<Arc<ReplayLog>>() {
        let path = ctx.path_node.map(|p| p.to_string_vec().join(".")).unwrap_or_default();
        log.0.lock().unwrap().push(path);
    }
}
