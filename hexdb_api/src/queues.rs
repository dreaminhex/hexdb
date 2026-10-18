// HexDB API: queues (message delivery to competing consumers; see hexdb_core::queues)
//
//   GET    /queues                          queues you can read
//   POST   /queues                          create (manage on queue:<name>)
//   GET    /queues/{name}                   configuration and counts (read)
//   PUT    /queues/{name}                   change the configuration (manage)
//   DELETE /queues/{name}                   delete it with its messages (manage)
//   POST   /queues/{name}/messages          {"messages": [{"body", "delay_seconds"?, "dedupe_key"?}]} (write)
//   POST   /queues/{name}/receive           {"max"?, "visibility_seconds"?, "wait_seconds"?} -> leased messages with receipts (read)
//   POST   /queues/{name}/ack               {"receipts": [...]}: done, delete them (read)
//   POST   /queues/{name}/nack              {"receipts", "delay_seconds"?}: give them back (read)
//   POST   /queues/{name}/extend            {"receipts", "visibility_seconds"}: keep them longer (read)
//
// Every call but the GETs changes messages, so replicas pass them to the Overseer.

use crate::auth::Auth;
use crate::handlers::{ApiError, ApiResult, Engine};
use axum::{
    extract::{rejection::JsonRejection, Path},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use hexdb_core::{
    queues::{self, NewQueueMessage, QueueConfig},
    Permission, Principal,
};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

fn require(principal: &Principal, permission: Permission, name: &str) -> Result<(), ApiError> {
    principal.require(permission, &queues::resource(name)).map_err(ApiError::from)
}

/// Messages that fail too often are moved to the dead-letter queue: that's
/// writing to it, so a queue may only name one its maker can write to.
fn require_dead_letter(principal: &Principal, config: &QueueConfig) -> Result<(), ApiError> {
    match config.dead_letter.as_deref() {
        Some(dead) => require(principal, Permission::Write, dead),
        None => Ok(()),
    }
}

pub async fn list(axum::extract::State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    let list: Vec<QueueConfig> = engine.list_queues().await?.into_iter().filter(|q| principal.can(Permission::Read, &queues::resource(&q.name))).collect();
    Ok(Json(json!({ "queues": list })).into_response())
}

pub async fn create(axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<QueueConfig>, JsonRejection>) -> ApiResult {
    let Json(config) = body?;
    require(&principal, Permission::Manage, &config.name)?;
    require_dead_letter(&principal, &config)?;
    let saved = engine.save_queue(config, &principal.login, true).await?;
    engine.audit(&principal.login, "queue.create", &saved.name, json!({})).await;
    Ok((StatusCode::CREATED, Json(json!(saved))).into_response())
}

pub async fn get(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    require(&principal, Permission::Read, &name)?;
    Ok(Json(engine.queue_stats(&name).await?).into_response())
}

pub async fn update(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<QueueConfig>, JsonRejection>) -> ApiResult {
    require(&principal, Permission::Manage, &name)?;
    let Json(mut config) = body?;
    config.name = name.clone();
    require_dead_letter(&principal, &config)?;
    let saved = engine.save_queue(config, &principal.login, false).await?;
    engine.audit(&principal.login, "queue.update", &name, json!({})).await;
    Ok(Json(json!(saved)).into_response())
}

pub async fn delete(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    require(&principal, Permission::Manage, &name)?;
    if !engine.delete_queue(&name).await? {
        return Err(ApiError::not_found(format!("Queue '{}' not found.", name)));
    }
    engine.audit(&principal.login, "queue.delete", &name, json!({})).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendBody {
    pub messages: Vec<NewQueueMessage>,
}

pub async fn send(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<SendBody>, JsonRejection>) -> ApiResult {
    require(&principal, Permission::Write, &name)?;
    let Json(body) = body?;
    let ids = engine.send_messages(&name, body.messages, &principal.login).await?;
    Ok((StatusCode::CREATED, Json(json!({ "ids": ids }))).into_response())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiveBody {
    #[serde(default)]
    pub max: Option<usize>,
    #[serde(default)]
    pub visibility_seconds: Option<u64>,
    #[serde(default)]
    pub wait_seconds: Option<u64>,
}

pub async fn receive(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth, body: Option<Json<ReceiveBody>>) -> ApiResult {
    require(&principal, Permission::Read, &name)?;
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let wait = Duration::from_secs(body.wait_seconds.unwrap_or(0).min(queues::MAX_WAIT_SECONDS));
    let messages = engine.receive_messages(&name, body.max.unwrap_or(1), body.visibility_seconds, wait).await?;
    Ok(Json(json!({ "messages": messages })).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptsBody {
    pub receipts: Vec<String>,
    #[serde(default)]
    pub delay_seconds: Option<u64>,
    #[serde(default)]
    pub visibility_seconds: Option<u64>,
}

pub async fn ack(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<ReceiptsBody>, JsonRejection>) -> ApiResult {
    require(&principal, Permission::Read, &name)?;
    let Json(body) = body?;
    let done = engine.ack_messages(&name, &body.receipts).await?;
    Ok(Json(json!({ "acked": done, "stale": body.receipts.len().saturating_sub(done) })).into_response())
}

pub async fn nack(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<ReceiptsBody>, JsonRejection>) -> ApiResult {
    require(&principal, Permission::Read, &name)?;
    let Json(body) = body?;
    let done = engine.release_messages(&name, &body.receipts, body.delay_seconds.unwrap_or(0), false).await?;
    Ok(Json(json!({ "released": done, "stale": body.receipts.len().saturating_sub(done) })).into_response())
}

pub async fn extend(Path(name): Path<String>, axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<ReceiptsBody>, JsonRejection>) -> ApiResult {
    require(&principal, Permission::Read, &name)?;
    let Json(body) = body?;
    let seconds = body.visibility_seconds.ok_or_else(|| ApiError::invalid("visibility_seconds is required."))?;
    let done = engine.release_messages(&name, &body.receipts, seconds, true).await?;
    Ok(Json(json!({ "extended": done, "stale": body.receipts.len().saturating_sub(done) })).into_response())
}
