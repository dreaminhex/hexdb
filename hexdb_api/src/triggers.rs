// HexDB API: triggers (see hexdb_core::triggers)
//
//   GET    /triggers            every trigger with its run status (admins)
//   POST   /triggers            create; it runs as you (admins)
//   GET    /triggers/{name}     one trigger with its status
//   PUT    /triggers/{name}     replace; it then runs as you (admins)
//   DELETE /triggers/{name}     delete (admins)

use crate::auth::Auth;
use crate::handlers::{ApiError, ApiResult, Engine};
use axum::{
    extract::{rejection::JsonRejection, Path},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use hexdb_core::{triggers::Trigger, EngineError, HexDBEngine};
use serde_json::{json, Value};

fn not_found(name: &str) -> ApiError {
    ApiError::from(anyhow::Error::from(EngineError::NotFound(format!("Trigger '{}' not found.", name))))
}

fn with_status(engine: &HexDBEngine, trigger: &Trigger) -> Value {
    let mut value = json!(trigger);
    value["status"] = json!(engine.trigger_status(&trigger.name));
    value
}

pub async fn list(axum::extract::State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    let list: Vec<Value> = engine.list_triggers().await?.iter().map(|t| with_status(&engine, t)).collect();
    Ok(Json(json!({ "triggers": list })).into_response())
}

pub async fn get(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    let trigger = engine.get_trigger(&name).await?.ok_or_else(|| not_found(&name))?;
    Ok(Json(with_status(&engine, &trigger)).into_response())
}

pub async fn create(axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<Trigger>, JsonRejection>) -> ApiResult {
    principal.require_admin()?;
    let Json(trigger) = body?;
    let saved = engine.save_trigger(trigger, &principal, true).await?;
    engine
        .audit(&principal.login, "trigger.create", &saved.name, json!({ "tessellation": saved.tessellation, "function": saved.function, "timing": saved.timing }))
        .await;
    Ok((StatusCode::CREATED, Json(with_status(&engine, &saved))).into_response())
}

pub async fn update(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<Trigger>, JsonRejection>) -> ApiResult {
    principal.require_admin()?;
    let Json(mut trigger) = body?;
    trigger.name = name.clone();
    let saved = engine.save_trigger(trigger, &principal, false).await?;
    engine.audit(&principal.login, "trigger.update", &name, json!({ "enabled": saved.enabled })).await;
    Ok(Json(with_status(&engine, &saved)).into_response())
}

pub async fn delete(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    if !engine.delete_trigger(&name).await? {
        return Err(not_found(&name));
    }
    engine.audit(&principal.login, "trigger.delete", &name, json!({})).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}
