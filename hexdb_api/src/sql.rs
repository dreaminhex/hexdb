// HexDB API: SQL (see hexdb_core::sql)
//
//   POST /sql   {"sql": "SELECT ...", "params": [...], "page_size": 1000, "cursor": "..."}
//               {"sql": "DELETE FROM t WHERE ...", "params": [...], "dry_run"?, "expect"?, "all"?}
//   GET  /sql/tables               tessellations the caller can read
//   GET  /sql/columns?table=NAME   their columns (schema, else sampled fields)
//
// Returns {"columns": [{"name", "type"}], "rows": [[...]], "next", "translated"}.
// `next` is null on the last page; send it back as `cursor`, with the same
// statement and parameters, for the next one. Reading needs read permission on
// the tessellation, and the caller's row filters and field masks apply.
//
// A DELETE runs as `POST /{t}/_delete` would, with the same permission (write;
// delete for a files tessellation), options and Idempotency-Key, and answers
// {"columns": [], "rows": [], "next": null, "rows_affected", "delete", "translated"}:
// `rows_affected` is how many of t's documents went (0 for a dry run), and
// `delete` the delete's summary (the batch to restore, the cascade).

use crate::auth::Auth;
use crate::handlers::{existing_tessellation, idempotency, respond, ApiError, ApiResult, Engine};
use axum::{
    extract::{rejection::JsonRejection, rejection::QueryRejection, Query, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::IntoResponse,
    Json,
};
use hexdb_core::engine::deletes::DeleteOptions;
use hexdb_core::{Permission, SqlDelete, SqlPage, SqlStatement, MAX_SQL_PAGE_SIZE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Tessellations the caller can read, for SQL clients' catalogs.
pub async fn tables(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    let mut names: Vec<String> = engine
        .tessellation_details()
        .into_iter()
        .filter(|(name, info)| info.is_user_data() && !name.starts_with('_') && principal.can(Permission::Read, name))
        .map(|(name, _)| name)
        .collect();
    names.sort();
    Ok(Json(json!({ "tables": names.iter().map(|n| json!({ "name": n })).collect::<Vec<_>>() })).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnsParams {
    pub table: String,
}

/// One tessellation's columns, as `SELECT *` would return them to the caller.
pub async fn columns(State(engine): Engine, Auth(principal): Auth, params: Result<Query<ColumnsParams>, QueryRejection>) -> ApiResult {
    let Query(params) = params?;
    principal.require(Permission::Read, &params.table)?;
    existing_tessellation(&engine, &params.table)?;
    let columns = engine.sql_columns(&params.table).await?;
    Ok(Json(json!({ "table": params.table, "columns": columns })).into_response())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SqlRequest {
    pub sql: String,
    #[serde(default)]
    pub params: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_size: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// DELETE: only say what would go.
    #[serde(default)]
    pub dry_run: bool,
    /// DELETE: refuse unless exactly this many documents and files would go.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<usize>,
    /// DELETE without a WHERE (or with one that's always true): yes, every document.
    #[serde(default)]
    pub all: bool,
}

pub async fn run(State(engine): Engine, Auth(principal): Auth, method: Method, uri: Uri, headers: HeaderMap, body: Result<Json<SqlRequest>, JsonRejection>) -> ApiResult {
    let Json(request) = body?;
    let query = match SqlStatement::parse(&request.sql, &request.params)? {
        SqlStatement::Query(query) => query,
        SqlStatement::Delete(delete) => return run_delete(&engine, &principal, &method, &uri, &headers, &request, delete).await,
    };
    if request.dry_run || request.all || request.expect.is_some() {
        return Err(ApiError::invalid("dry_run, expect and all are for DELETE statements."));
    }
    // Every table the statement reads (joins, subqueries, CTEs) needs read permission.
    for tess in query.tessellations() {
        principal.require(Permission::Read, tess)?;
        existing_tessellation(&engine, tess)?;
    }
    let size = request.page_size.unwrap_or(0);
    if request.page_size.is_some() && !(1..=MAX_SQL_PAGE_SIZE).contains(&size) {
        return Err(ApiError::invalid(format!("page_size must be 1-{}.", MAX_SQL_PAGE_SIZE)));
    }
    let result = engine.sql(&query, &SqlPage { size, cursor: request.cursor }).await?;
    Ok(Json(json!({
        "columns": result.columns,
        "rows": result.rows,
        "next": result.next,
        "translated": query.describe(),
    }))
    .into_response())
}

/// `DELETE FROM t WHERE ...`: a delete by filter, answered in SQL's shape.
async fn run_delete(
    engine: &std::sync::Arc<hexdb_core::HexDBEngine>,
    principal: &hexdb_core::Principal,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    request: &SqlRequest,
    delete: SqlDelete,
) -> ApiResult {
    let tess = delete.tessellation.as_str();
    // page_size is ignored (drivers send it with every statement); a cursor only continues a SELECT.
    if request.cursor.is_some() {
        return Err(ApiError::invalid("cursor is for SELECT statements."));
    }
    let files = engine.tessellation_info(tess).is_some_and(|i| i.is_files());
    principal.require(if files { Permission::Delete } else { Permission::Write }, tess)?;
    existing_tessellation(engine, tess)?;
    let options = DeleteOptions { all: request.all, dry_run: request.dry_run, expect: request.expect };
    let body = serde_json::to_value(request).map_err(|e| ApiError::invalid(e.to_string()))?;
    let idem = if request.dry_run { None } else { idempotency(principal, headers, method, uri, Some(&body))? };
    let outcome = engine.delete_where(tess, &delete.filter, options, idem).await?;
    let summary = outcome.value;
    if let (Some(batch), false) = (&summary.batch, outcome.replayed) {
        let details = json!({ "batch": batch, "total": summary.total, "deleted": summary.deleted, "detached": summary.detached, "all": request.all, "sql": true });
        engine.audit(&principal.login, "data.delete.filter", tess, details).await;
    }
    let affected = if summary.dry_run { 0 } else { summary.matched };
    let body = json!({
        "columns": [],
        "rows": [],
        "next": null,
        "rows_affected": affected,
        "delete": summary,
        "translated": delete.describe(),
    });
    Ok(respond(StatusCode::OK, body, outcome.replayed))
}
