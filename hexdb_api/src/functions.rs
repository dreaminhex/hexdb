// HexDB API: functions and schedules (see hexdb_core::functions)
//
//   GET    /functions                  every function (signed-in users)
//   POST   /functions                  create (admins)
//   GET    /functions/{name}           one function
//   PUT    /functions/{name}           replace (admins)
//   DELETE /functions/{name}           delete (admins)
//   POST   /functions/{name}/run       {"params": {...}}: run it with your permissions
//   GET    /schedules                  every schedule (admins)
//   POST   /schedules                  create; it runs as you (admins)
//   GET/PUT/DELETE /schedules/{name}
//   POST   /schedules/{name}/run       run it now

use crate::auth::Auth;
use crate::handlers::{ApiError, ApiResult, Engine};
use axum::{
    extract::{rejection::JsonRejection, Path},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use hexdb_core::{
    functions::{FunctionDef, ScheduleDef},
    EngineError,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};

fn not_found(what: &str, name: &str) -> ApiError {
    ApiError::from(anyhow::Error::from(EngineError::NotFound(format!("{} '{}' not found.", what, name))))
}

pub async fn list(axum::extract::State(engine): Engine, Auth(_principal): Auth) -> ApiResult {
    Ok(Json(json!({ "functions": engine.list_functions().await? })).into_response())
}

pub async fn get(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(_principal): Auth) -> ApiResult {
    let def = engine.get_function(&name).await?.ok_or_else(|| not_found("Function", &name))?;
    Ok(Json(json!(def)).into_response())
}

pub async fn create(axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<FunctionDef>, JsonRejection>) -> ApiResult {
    principal.require_admin()?;
    let Json(def) = body?;
    let saved = engine.save_function(def, &principal.login, true).await?;
    engine.audit(&principal.login, "function.create", &saved.name, json!({ "kind": saved.kind })).await;
    Ok((StatusCode::CREATED, Json(json!(saved))).into_response())
}

pub async fn update(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<FunctionDef>, JsonRejection>) -> ApiResult {
    principal.require_admin()?;
    let Json(mut def) = body?;
    def.name = name.clone();
    let saved = engine.save_function(def, &principal.login, false).await?;
    engine.audit(&principal.login, "function.update", &name, json!({ "kind": saved.kind })).await;
    Ok(Json(json!(saved)).into_response())
}

pub async fn delete(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    if !engine.delete_function(&name).await? {
        return Err(not_found("Function", &name));
    }
    engine.audit(&principal.login, "function.delete", &name, json!({})).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRequest {
    #[serde(default)]
    pub params: Map<String, Value>,
}

/// Run a function with the caller's permissions.
pub async fn run(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth, body: Option<Json<RunRequest>>) -> ApiResult {
    let def = engine.get_function(&name).await?.ok_or_else(|| not_found("Function", &name))?;
    let params = body.map(|Json(b)| b.params).unwrap_or_default();
    let started = std::time::Instant::now();
    let result = engine.run_function(&def, &principal, &params).await?;
    if def.kind == "script" {
        engine.audit(&principal.login, "function.run", &name, json!({ "ms": started.elapsed().as_millis() as u64 })).await;
    }
    Ok(Json(json!({ "result": result, "ms": started.elapsed().as_millis() as u64 })).into_response())
}

pub async fn list_schedules(axum::extract::State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    Ok(Json(json!({ "schedules": engine.list_schedules().await? })).into_response())
}

pub async fn get_schedule(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    let schedule = engine.get_schedule(&name).await?.ok_or_else(|| not_found("Schedule", &name))?;
    Ok(Json(json!(schedule)).into_response())
}

pub async fn create_schedule(axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<ScheduleDef>, JsonRejection>) -> ApiResult {
    principal.require_admin()?;
    let Json(schedule) = body?;
    let saved = engine.save_schedule(schedule, &principal, true).await?;
    engine.audit(&principal.login, "schedule.create", &saved.name, json!({ "function": saved.function })).await;
    Ok((StatusCode::CREATED, Json(json!(saved))).into_response())
}

pub async fn update_schedule(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<ScheduleDef>, JsonRejection>) -> ApiResult {
    principal.require_admin()?;
    let Json(mut schedule) = body?;
    schedule.name = name.clone();
    let saved = engine.save_schedule(schedule, &principal, false).await?;
    engine.audit(&principal.login, "schedule.update", &name, json!({ "function": saved.function })).await;
    Ok(Json(json!(saved)).into_response())
}

pub async fn delete_schedule(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    if !engine.delete_schedule(&name).await? {
        return Err(not_found("Schedule", &name));
    }
    engine.audit(&principal.login, "schedule.delete", &name, json!({})).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Run a schedule now (as its owner) and return the function's result.
pub async fn run_schedule(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    let schedule = engine.get_schedule(&name).await?.ok_or_else(|| not_found("Schedule", &name))?;
    let result = engine.run_schedule(schedule).await?;
    Ok(Json(json!({ "result": result })).into_response())
}
