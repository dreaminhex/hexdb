// HexDB API: the lattice configuration (admins only; see `hexdb_core::lattice_config`)
//
//   GET  /config                     the newest version, the schema, and every hex's state
//   PUT  /config                     change settings: {"changes": {key: value | null}, "hex"?, "comment"?, "base_version"?}
//   GET  /config/history             versions, newest first, with what each changed (?limit=&offset=)
//   GET  /config/versions/{version}  one version
//   POST /config/rollback            {"version": n, "comment"?}: a new version with version n's contents
//   POST /config/storage-test        write, read back and delete a small object on this hex's storage backend (admins)
//   GET  /ai                         the AI provider in use and this month's token usage (admins)
//   POST /ai/test                    a tiny call that shows whether the provider, model and key work (admins)
//   POST /ai/complete                {"prompt", "system"?, "max_tokens"?}: run a prompt (the `ai` permission)
//   GET  /ai/estimate                token estimates for AI tagging and video thumbnails, per files tessellation (admins)
//   POST /{tess}/_ai_tag             tag files not tagged for their current content (?id= one file, ?all=true every file) (admins)
//   POST /{tess}/_ai_thumbnails      let the AI choose video thumbnails, the same way (admins)
//
// Secret values are accepted by PUT and never returned, logged or audited.
// With `security.demo_mode`, any signed-in user may view the configuration,
// its history, AI usage and estimates; everything else stays admin-only.
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

/// Viewing: administrators, or anyone signed in when the hex runs in demo mode.
fn require_view(engine: &hexdb_core::engine::HexDBEngine, principal: &hexdb_core::Principal) -> Result<(), ApiError> {
    if engine.config.security.demo_mode {
        return Ok(());
    }
    principal.require_admin().map_err(Into::into)
}

pub async fn get_config(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    require_view(&engine, &principal)?;
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
    require_view(&engine, &principal)?;
    let Query(params) = params?;
    let (versions, total) = engine.lattice_config_history(params.limit.unwrap_or(50).clamp(1, 500), params.offset.unwrap_or(0)).await?;
    Ok(Json(json!({ "versions": versions, "total": total })).into_response())
}

pub async fn config_version(Path(version): Path<String>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    require_view(&engine, &principal)?;
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

pub async fn ai_status(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    require_view(&engine, &principal)?;
    Ok(Json(json!({ "ai": engine.ai_summary(), "usage": engine.ai_usage_view().await? })).into_response())
}

pub async fn ai_test(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    let started = engine.ai_summary();
    match engine.ai_test().await {
        Ok(reply) => Ok(Json(json!({ "ok": true, "reply": reply.text.chars().take(200).collect::<String>(), "provider": reply.provider, "model": reply.model, "input_tokens": reply.input_tokens, "output_tokens": reply.output_tokens, "millis": reply.millis })).into_response()),
        // Not configured: a plain 400.
        Err(e) if matches!(e.downcast_ref::<hexdb_core::EngineError>(), Some(hexdb_core::EngineError::Invalid(_))) => Err(e.into()),
        Err(e) => Ok(Json(json!({ "ok": false, "provider": started.get("provider"), "model": started.get("model"), "error": format!("{:#}", e) })).into_response()),
    }
}

/// A provider's failure as 502, unless it's one of ours (not configured, budget, rate limit).
fn provider_failure(e: anyhow::Error) -> ApiError {
    if e.downcast_ref::<hexdb_core::EngineError>().is_some() {
        return e.into();
    }
    ApiError::new(axum::http::StatusCode::BAD_GATEWAY, "ai_provider_error", format!("{:#}", e))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteRequest {
    pub prompt: String,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
}

/// Run a prompt through the lattice's AI provider, for plugins and scripts
/// (the `ai` permission). Usage is recorded under `api:<login>`.
pub async fn ai_complete(State(engine): Engine, Auth(principal): Auth, body: Result<Json<CompleteRequest>, JsonRejection>) -> ApiResult {
    principal.require_action(hexdb_core::Action::Ai)?;
    let Json(request) = body?;
    if request.prompt.trim().is_empty() || request.prompt.len() > 400_000 {
        return Err(ApiError::invalid("prompt must be 1 to 400,000 bytes."));
    }
    let reply = engine
        .ai_complete(hexdb_core::ai::AiRequest {
            feature: format!("api:{}", principal.login),
            system: request.system.unwrap_or_default(),
            parts: vec![hexdb_core::ai::Part::Text(request.prompt)],
            max_tokens: request.max_tokens.unwrap_or(4000),
        })
        .await
        .map_err(provider_failure)?;
    Ok(Json(json!(reply)).into_response())
}

pub async fn ai_estimate(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    require_view(&engine, &principal)?;
    Ok(Json(engine.ai_estimate().await?).into_response())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AiFilesParams {
    pub id: Option<String>,
    #[serde(default)]
    pub all: bool,
}

async fn queue_ai(
    engine: &hexdb_core::engine::HexDBEngine,
    login: &str,
    tess: &str,
    task: hexdb_core::engine::AiTask,
    params: AiFilesParams,
) -> ApiResult {
    let queued = engine.queue_ai_files(tess, task, params.id.as_deref(), params.all).await?;
    let action = if task == hexdb_core::engine::AiTask::Tag { "ai.tag" } else { "ai.thumbnails" };
    engine.audit(login, action, tess, json!({ "queued": queued, "id": params.id, "all": params.all })).await;
    Ok((axum::http::StatusCode::ACCEPTED, Json(json!({ "queued": queued }))).into_response())
}

/// Tag existing files with the AI (it spends tokens, so administrators only).
pub async fn ai_tag(Path(tess): Path<String>, params: Result<Query<AiFilesParams>, QueryRejection>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    let Query(params) = params?;
    queue_ai(&engine, &principal.login, &tess, hexdb_core::engine::AiTask::Tag, params).await
}

/// Let the AI choose existing videos' thumbnails (administrators only).
pub async fn ai_thumbnails(Path(tess): Path<String>, params: Result<Query<AiFilesParams>, QueryRejection>, State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    let Query(params) = params?;
    queue_ai(&engine, &principal.login, &tess, hexdb_core::engine::AiTask::Thumbnail, params).await
}

/// Check the storage backend the saved configuration chooses for this hex.
pub async fn storage_test(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    Ok(Json(engine.test_file_storage().await).into_response())
}
