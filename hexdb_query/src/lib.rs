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
    validate_tessellation_name, Document, DocumentQuery, EngineError, Filter, HexDBEngine, SortKey, TessellationInfo,
};
use serde_json::Value;
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
        None => {
            error!("❌ GraphQL request failed: {:#}", e);
            gql_error("INTERNAL", "The request failed; see the server log.")
        }
    }
}

fn engine<'a>(ctx: &Context<'a>) -> &'a Arc<HexDBEngine> {
    ctx.data_unchecked::<Arc<HexDBEngine>>()
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
    engine: &HexDBEngine,
    tess: &str,
    filter: Option<Json<Value>>,
    sort: Option<Vec<SortInput>>,
    limit: Option<i32>,
    offset: Option<i32>,
    after: Option<ID>,
) -> GqlResult<DocumentPage> {
    existing_tessellation(engine, tess)?;
    let limit = limit.unwrap_or(100);
    if limit < 0 || limit as usize > MAX_PAGE_SIZE {
        return Err(gql_error("INVALID_REQUEST", format!("limit must be 0-{}.", MAX_PAGE_SIZE)));
    }
    let query = DocumentQuery {
        filter: parse_filter(filter)?,
        sort: sort.unwrap_or_default().into_iter().map(Into::into).collect(),
        offset: offset.unwrap_or(0).max(0) as usize,
        limit: limit as usize,
        after: parse_after(after)?,
    };
    let page = engine.query_documents(tess, &query).await.map_err(to_gql)?;
    Ok(DocumentPage {
        documents: page.documents.into_iter().map(DocumentObject).collect(),
        total: page.total,
        next: page.next.map(|id| ID(id.to_string())),
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
    async fn data(&self) -> Json<Value> {
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
    /// Documents matching the filter, across all pages.
    pub total: usize,
    /// Pass as `after` for the next page (unsorted queries only).
    pub next: Option<ID>,
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

    /// Number of documents, optionally only those matching a filter.
    async fn document_count(&self, ctx: &Context<'_>, filter: Option<Json<Value>>) -> GqlResult<usize> {
        let engine = engine(ctx);
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
        query_page(engine(ctx), &self.name, filter, sort, limit, offset, after).await
    }
}

/// Result of `updateDocuments`.
#[derive(SimpleObject)]
pub struct UpdateResult {
    pub matched: usize,
    pub modified: usize,
}

/// A role granted to a user.
#[derive(SimpleObject)]
pub struct RoleGrantObject {
    pub name: String,
    pub permissions: Vec<String>,
}

impl From<RoleGrant> for RoleGrantObject {
    fn from(g: RoleGrant) -> Self {
        RoleGrantObject { name: g.name, permissions: g.permissions }
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
}

impl From<RoleView> for RoleObject {
    fn from(r: RoleView) -> Self {
        RoleObject { id: ID(r.id), name: r.name, description: r.description }
    }
}

// ---------------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct QueryRoot;

#[Object]
impl QueryRoot {
    /// All tessellations.
    async fn tessellations(&self, ctx: &Context<'_>) -> Vec<TessellationObject> {
        engine(ctx)
            .tessellation_details()
            .into_iter()
            .map(|(name, info)| TessellationObject { name, info })
            .collect()
    }

    /// One tessellation by name.
    async fn tessellation(&self, ctx: &Context<'_>, name: String) -> Option<TessellationObject> {
        engine(ctx).tessellation_info(&name).map(|info| TessellationObject { name, info })
    }

    /// One document by ID.
    async fn document(&self, ctx: &Context<'_>, tessellation: String, id: ID) -> GqlResult<Option<DocumentObject>> {
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
        query_page(engine(ctx), &tessellation, filter, sort, limit, offset, after).await
    }

    /// Number of documents, optionally only those matching a filter.
    async fn count(&self, ctx: &Context<'_>, tessellation: String, filter: Option<Json<Value>>) -> GqlResult<usize> {
        let engine = engine(ctx);
        existing_tessellation(engine, &tessellation)?;
        engine.count_matching(&tessellation, &parse_filter(filter)?).await.map_err(to_gql)
    }

    /// All users.
    async fn users(&self, ctx: &Context<'_>) -> GqlResult<Vec<UserObject>> {
        Ok(users::list_users(engine(ctx)).await.map_err(to_gql)?.into_iter().map(Into::into).collect())
    }

    /// One user by ID or login.
    async fn user(&self, ctx: &Context<'_>, id_or_login: String) -> GqlResult<Option<UserObject>> {
        Ok(users::get_user(engine(ctx), &id_or_login).await.map_err(to_gql)?.map(Into::into))
    }

    /// All roles.
    async fn roles(&self, ctx: &Context<'_>) -> GqlResult<Vec<RoleObject>> {
        Ok(users::list_roles(engine(ctx)).await.map_err(to_gql)?.into_iter().map(Into::into).collect())
    }

    /// Server status and metrics (the same data as GET /status).
    async fn status(&self, ctx: &Context<'_>) -> GqlResult<Json<Value>> {
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
impl MutationRoot {
    /// Insert a document. `ttl` is in seconds.
    async fn insert_document(
        &self,
        ctx: &Context<'_>,
        tessellation: String,
        data: Json<Value>,
        ttl: Option<u64>,
    ) -> GqlResult<DocumentObject> {
        let engine = engine(ctx);
        user_tessellation(engine, &tessellation)?;
        let mut docs = engine
            .insert_documents(&tessellation, vec![data.0], expiry(ttl), None)
            .await
            .map_err(to_gql)?
            .value;
        Ok(DocumentObject(docs.remove(0)))
    }

    /// Insert many documents atomically.
    async fn insert_documents(
        &self,
        ctx: &Context<'_>,
        tessellation: String,
        documents: Vec<Json<Value>>,
        ttl: Option<u64>,
    ) -> GqlResult<Vec<DocumentObject>> {
        let engine = engine(ctx);
        user_tessellation(engine, &tessellation)?;
        let docs = documents.into_iter().map(|d| d.0).collect();
        let outcome = engine.insert_documents(&tessellation, docs, expiry(ttl), None).await.map_err(to_gql)?;
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
    ) -> GqlResult<DocumentObject> {
        let engine = engine(ctx);
        user_tessellation(engine, &tessellation)?;
        let outcome = engine
            .replace_document(&tessellation, &id, data.0, expiry(ttl), None)
            .await
            .map_err(to_gql)?;
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
    ) -> GqlResult<DocumentObject> {
        let engine = engine(ctx);
        user_tessellation(engine, &tessellation)?;
        let outcome = engine
            .patch_document(&tessellation, &id, data.0, expiry(ttl), None)
            .await
            .map_err(to_gql)?;
        Ok(DocumentObject(outcome.value))
    }

    /// Delete a document. Returns false if it didn't exist.
    async fn delete_document(&self, ctx: &Context<'_>, tessellation: String, id: ID) -> GqlResult<bool> {
        let engine = engine(ctx);
        user_tessellation(engine, &tessellation)?;
        Ok(engine.delete_document(&tessellation, &id, None).await.map_err(to_gql)?.value)
    }

    /// Merge-patch every document matching a filter, atomically.
    async fn update_documents(
        &self,
        ctx: &Context<'_>,
        tessellation: String,
        filter: Json<Value>,
        update: Json<Value>,
        ttl: Option<u64>,
    ) -> GqlResult<UpdateResult> {
        let engine = engine(ctx);
        existing_tessellation(engine, &tessellation)?;
        let outcome = engine
            .update_where(&tessellation, &filter.0, &update.0, expiry(ttl), None)
            .await
            .map_err(to_gql)?;
        Ok(UpdateResult { matched: outcome.value.matched, modified: outcome.value.modified })
    }

    /// Create a user tessellation.
    async fn create_tessellation(&self, ctx: &Context<'_>, name: String) -> GqlResult<TessellationObject> {
        let engine = engine(ctx);
        user_tessellation(engine, &name)?;
        if !engine.create_tessellation(&name, "user").map_err(to_gql)? {
            return Err(gql_error("CONFLICT", format!("Tessellation '{}' already exists.", name)));
        }
        let info = engine
            .tessellation_info(&name)
            .ok_or_else(|| gql_error("INTERNAL", "Tessellation vanished."))?;
        Ok(TessellationObject { name, info })
    }

    /// Delete a user tessellation and all of its documents. Returns false if it didn't exist.
    async fn delete_tessellation(&self, ctx: &Context<'_>, name: String) -> GqlResult<bool> {
        let engine = engine(ctx);
        if engine.tessellation_exists(&name) && engine.is_system_tessellation(&name) {
            return Err(gql_error("FORBIDDEN", format!("'{}' is a system tessellation and can't be deleted.", name)));
        }
        engine.delete_tessellation(&name).await.map_err(to_gql)
    }
}
