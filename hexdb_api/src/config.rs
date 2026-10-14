// HexDB API: the lattice configuration (admins only; see `hexdb_core::lattice_config`)
//
//   GET  /config                     the newest version, the schema, and every hex's state
//   PUT  /config                     change settings: {"changes": {key: value | null}, "hex"?, "comment"?, "base_version"?}
//   GET  /config/history             versions, newest first, with what each changed (?limit=&offset=)
//   GET  /config/versions/{version}  one version
//   POST /config/rollback            {"version": n, "comment"?}: a new version with version n's contents
//
// Secret values are accepted by PUT and never returned, logged or audited.
// Writes are forwarded to the Overseer like other writes.

use crate::auth::Auth;
use crate::handlers::{ApiError, ApiResult, Engine};
use axum::{
    extract::{
        rejection::{JsonRejection, QueryRejection},
        Path, Query, State,
    },
    response::IntoResponse,
    Json,
};
use hexdb_core::lattice_config::ConfigChange;
use serde::Deserialize;
use serde_json::json;
use tracing::info;

pub async fn get_config(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    Ok(Json(engine.lattice_config_view().await?).into_response())
}

pub async fn put_config(State(engine): Engine, Auth(principal): Auth, body: Result<Json<ConfigChange>, JsonRejection>) -> ApiResult {
    principal.require_admin()?;
    let Json(request) = body?;
    let (version, summary) = engine.update_lattice_config(&principal.login, request).await?;
    if !is_noop(&summary) {
        let target = summary.hex.clone().unwrap_or_else(|| "lattice".into());
        engine.audit(&principal.login, "config.update", &target, json!(summary)).await;
        info!("⚙️ '{}' saved lattice configuration version {}.", principal.login, version.version);
    }
    let mut view = engine.lattice_config_view().await?;
    view["changes"] = json!(summary);
    Ok(Json(view).into_response())
}

fn is_noop(summary: &hexdb_core::lattice_config::ChangeSummary) -> bool {
    summary.changed.is_empty() && summary.cleared.is_empty() && summary.secrets_set.is_empty() && summary.secrets_cleared.is_empty()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryParams {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

pub async fn config_history(params: Result<Query<HistoryParams>, QueryRejection>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    let Query(params) = params?;
    let (versions, total) = engine.lattice_config_history(params.limit.unwrap_or(50).clamp(1, 500), params.offset.unwrap_or(0)).await?;
    Ok(Json(json!({ "versions": versions, "total": total })).into_response())
}

pub async fn config_version(Path(version): Path<String>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    let version: u64 = version.parse().map_err(|_| ApiError::invalid("The version must be a number."))?;
    Ok(Json(engine.lattice_config_version(version).await?).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackRequest {
    pub version: u64,
    pub comment: Option<String>,
}

pub async fn rollback_config(State(engine): Engine, Auth(principal): Auth, body: Result<Json<RollbackRequest>, JsonRejection>) -> ApiResult {
    principal.require_admin()?;
    let Json(request) = body?;
    let version = engine.rollback_lattice_config(&principal.login, request.version, request.comment).await?;
    engine
        .audit(&principal.login, "config.rollback", "lattice", json!({ "version": version.version, "restored": request.version }))
        .await;
    info!("⚙️ '{}' rolled the lattice configuration back to version {} (now version {}).", principal.login, request.version, version.version);
    Ok(Json(engine.lattice_config_view().await?).into_response())
}
