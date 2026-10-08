// HexDB API: SQL (see hexdb_core::sql)
//
//   POST /sql   {"sql": "SELECT ...", "params": [...], "page_size": 1000, "cursor": "..."}
//   GET  /sql/tables               tessellations the caller can read
//   GET  /sql/columns?table=NAME   their columns (schema, else sampled fields)
//
// Returns {"columns": [{"name", "type"}], "rows": [[...]], "next", "translated"}.
// `next` is null on the last page; send it back as `cursor`, with the same
// statement and parameters, for the next one. Reading needs read permission on
// the tessellation, and the caller's row filters and field masks apply.

use crate::auth::Auth;
use crate::handlers::{existing_tessellation, ApiError, ApiResult, Engine};
use axum::{
    extract::{rejection::JsonRejection, rejection::QueryRejection, Query, State},
    response::IntoResponse,
    Json,
};
use hexdb_core::{Permission, SqlPage, SqlQuery, MAX_SQL_PAGE_SIZE};
use serde::Deserialize;
use serde_json::{json, Value};

/// Tessellations the caller can read, for SQL clients' catalogs.
pub async fn tables(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    let mut names: Vec<String> = engine
        .tessellation_details()
        .into_iter()
        .filter(|(name, info)| info.kind == "user" && !name.starts_with('_') && principal.can(Permission::Read, name))
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlRequest {
    pub sql: String,
    #[serde(default)]
    pub params: Vec<Value>,
    pub page_size: Option<usize>,
    pub cursor: Option<String>,
}

pub async fn run(State(engine): Engine, Auth(principal): Auth, body: Result<Json<SqlRequest>, JsonRejection>) -> ApiResult {
    let Json(request) = body?;
    let query = SqlQuery::parse(&request.sql, &request.params)?;
    if let Some(tess) = query.tessellation() {
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
