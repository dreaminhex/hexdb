// HexDB API: AI security reviews and outbound email (see hexdb_core::review)
//
//   GET  /security/reviews       the newest reviews (administrators)
//   POST /security/reviews       review the last security.review.interval_hours now
//   POST /security/email-test    send a test email ({"to"?}; default: your own address)
//   POST /lattice/security-review   an Oracle's review, kept on the Overseer

use crate::auth::Auth;
use crate::handlers::{ApiError, ApiResult, Engine};
use axum::{
    body::Bytes,
    extract::{rejection::JsonRejection, Query},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
pub struct ListParams {
    limit: Option<usize>,
}

/// `GET /security/reviews?limit=`: newest first, with whether reviews are on and when the next is due.
pub async fn list(axum::extract::State(engine): Engine, Auth(principal): Auth, Query(params): Query<ListParams>) -> ApiResult {
    principal.require_admin()?;
    let reviews = engine.security_reviews(params.limit.unwrap_or(50).clamp(1, 500)).await?;
    let interval = engine.config_value("security.review.interval_hours").as_i64().unwrap_or(24);
    let enabled = engine.config_value("security.review.enabled") == json!(true);
    let next = reviews.first().map(|r| r.started + interval * 3_600_000).filter(|_| enabled);
    Ok(Json(json!({
        "enabled": enabled,
        "interval_hours": interval,
        "email": hexdb_core::email::configured(&engine),
        "ai": engine.ai_summary()["configured"],
        "next_due": next,
        "reviews": reviews,
    }))
    .into_response())
}

/// `POST /security/reviews`: review now (an AI call; takes a little while).
pub async fn run(axum::extract::State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    principal.require_admin()?;
    let review = engine.run_security_review(&principal.login).await?;
    Ok(Json(review).into_response())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailTest {
    #[serde(default)]
    to: Option<String>,
}

/// `POST /security/email-test`: check `email.*` with a short message.
pub async fn email_test(axum::extract::State(engine): Engine, Auth(principal): Auth, body: Result<Json<EmailTest>, JsonRejection>) -> ApiResult {
    principal.require_admin()?;
    let input = body.map(|Json(b)| b).unwrap_or_default();
    let to = input.to.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).unwrap_or_else(|| principal.email_address.clone());
    if to.is_empty() {
        return Err(ApiError::invalid("Give an address to send it to (your account has no email address)."));
    }
    let text = format!("This is a test email from HexDB lattice '{}', sent by {}. Outbound email works.", engine.config.network.lattice_name, principal.login);
    if let Err(e) = hexdb_core::email::send(&engine, std::slice::from_ref(&to), "HexDB test email", &text).await {
        return Err(ApiError::new(StatusCode::BAD_GATEWAY, "email_failed", format!("{:#}", e)));
    }
    engine.audit(&principal.login, "email.test", &to, json!({})).await;
    Ok(Json(json!({ "sent_to": to })).into_response())
}

/// `POST /lattice/security-review`: keep a review an Oracle made.
pub async fn store(method: Method, uri: Uri, headers: HeaderMap, axum::extract::State(engine): Engine, body: Bytes) -> ApiResult {
    crate::lattice::authorize(&engine, &headers, &method, &uri, &body)?;
    let review: hexdb_core::review::Review = serde_json::from_slice(&body).map_err(|e| ApiError::invalid(e.to_string()))?;
    engine.store_review_here(&review).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
