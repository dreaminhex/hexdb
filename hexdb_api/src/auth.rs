// HexDB API: authentication, CSRF protection, and security headers
//
// `authenticate` runs on every request. Requests to anything outside the
// public list need a valid session (cookie or bearer token) or API key, and
// get 401 otherwise: access is denied by default, so a new route can't be
// exposed by accident. Handlers then check permissions with the `Auth`
// extractor (see `hexdb_core::auth` for the permission model).
//
// Public: GET /health, GET /openapi.json, POST /auth/login, the admin UI's static files, the
// discovery documents under /.well-known/, the hex-to-hex /lattice/* endpoints (they verify the lattice signature
// themselves), and POST /shutdown (it verifies the shutdown token, or an
// admin session).
//
// CSRF: the session cookie is SameSite=Strict, and a state-changing request
// authenticated by the cookie must also come from the same origin (Origin, or
// Sec-Fetch-Site, must match). Bearer tokens aren't sent by browsers on their
// own, so they need no such check.

use crate::handlers::{ApiError, ApiResult, Engine};
use axum::{
    extract::{ConnectInfo, FromRequestParts, Path, Request, State},
    http::{header, request::Parts, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use hexdb_core::{auth as core_auth, engine::HexDBEngine, users, AuditEvent, Principal, SESSION_COOKIE};
use serde::Deserialize;
use serde_json::json;
use std::{net::SocketAddr, sync::Arc};
use tracing::info;

/// The authenticated principal, for handlers. Fails with 401 when absent
/// (the middleware only lets unauthenticated requests reach public routes).
pub struct Auth(pub Principal);

impl<S: Send + Sync> FromRequestParts<S> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Principal>()
            .cloned()
            .map(Auth)
            .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "Sign in, or send an API key."))
    }
}

/// The principal if the request is authenticated (for public routes that show more to signed-in users).
pub struct MaybeAuth(pub Option<Principal>);

impl<S: Send + Sync> FromRequestParts<S> for MaybeAuth {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(MaybeAuth(parts.extensions.get::<Principal>().cloned()))
    }
}

/// How the request authenticated (for the CSRF check and sign-out).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Via {
    Cookie,
    Bearer,
}

fn is_public(method: &Method, path: &str) -> bool {
    path == "/"
        || crate::routes::is_ui_path(path)
        || (path == "/health" && method == Method::GET)
        || (path == "/openapi.json" && method == Method::GET)
        // A2A discovery; it says nothing unless a2a.enabled, and /a2a itself needs credentials.
        || ((path == "/.well-known/agent-card.json" || path == crate::a2a::JWKS_PATH) && method == Method::GET)
        // OAuth discovery (RFC 9728); 404 unless oauth.issuer is set.
        || (path == PROTECTED_RESOURCE_PATH && method == Method::GET)
        || (path == "/auth/login" && method == Method::POST)
        || (path == "/shutdown" && method == Method::POST)
        || is_lattice_route(method, path)
        || is_share_route(method, path)
}

/// Sharing links (`/_s/<code>...`; tessellation names can't start with `_`, so these never clash), for people without an account; they check the link themselves.
fn is_share_route(method: &Method, path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/_s/") else { return false };
    let (code, tail) = rest.split_once('/').unwrap_or((rest, ""));
    if code.is_empty() || code.len() > 16 {
        return false;
    }
    match *method {
        Method::GET | Method::HEAD => matches!(tail, "" | "content" | "thumbnail" | "files"),
        Method::POST => tail == "unlock",
        _ => false,
    }
}

/// The hex-to-hex routes, which check their own lattice signatures. Listed
/// exactly, so no other handler is ever reached without credentials.
fn is_lattice_route(method: &Method, path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/lattice/") else { return false };
    let one_segment = |prefix: &str| rest.strip_prefix(prefix).is_some_and(|s| !s.is_empty() && !s.contains('/'));
    match *method {
        Method::GET => {
            matches!(rest, "snapshot" | "catalog" | "changes") || one_segment("snapshot/") || one_segment("slice-snapshot/") || one_segment("chunks/")
        }
        Method::POST => matches!(rest, "revoke" | "audit" | "ai-usage" | "throttle" | "share-download"),
        _ => false,
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token.trim())
}

/// The session cookie's value, if present.
pub fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| pair.trim().strip_prefix(SESSION_COOKIE).and_then(|rest| rest.strip_prefix('=')))
        .filter(|v| !v.is_empty())
}

/// The bearer token or session cookie a request carries.
pub fn request_credential(headers: &HeaderMap) -> Option<String> {
    bearer(headers).or_else(|| session_cookie(headers)).map(String::from)
}

/// True if a browser request came from this server's own pages.
pub(crate) fn same_origin(headers: &HeaderMap) -> bool {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let origin_host = origin.split_once("://").map(|(_, rest)| rest).unwrap_or(origin);
        return host.is_some_and(|h| h.eq_ignore_ascii_case(origin_host));
    }
    // No Origin: modern browsers still send Sec-Fetch-Site.
    matches!(
        headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()),
        Some("same-origin") | Some("none")
    )
}

fn unsafe_method(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// The client's address (see `crate::client` for trusted proxies).
pub fn client_address(parts: &axum::http::Extensions) -> String {
    match parts.get::<crate::client::ClientIp>() {
        Some(ip) => ip.0.clone(),
        None => parts.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_string()).unwrap_or_else(|| "unknown".into()),
    }
}

/// Authenticate every request; refuse non-public routes without credentials.
pub async fn authenticate(State(engine): State<Arc<HexDBEngine>>, mut request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let headers = request.headers();

    let (token, via) = match (bearer(headers), session_cookie(headers)) {
        (Some(t), _) => (Some(t.to_string()), Via::Bearer),
        (None, Some(c)) => (Some(c.to_string()), Via::Cookie),
        (None, None) => (None, Via::Bearer),
    };

    // A replica forwarding a write already checked its origin; the Origin
    // names the replica, not this hex.
    let forwarded = request.extensions().get::<crate::forward::Forwarded>().is_some();

    // A cross-site page must not be able to sign a browser in (login CSRF),
    // unless an administrator listed its origin in cors.allowed_origins.
    let foreign_origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()).filter(|_| !same_origin(headers));
    if path == "/auth/login" && foreign_origin.is_some_and(|o| !crate::cors::AllowedOrigins::from_engine(&engine).lists(o)) {
        return ApiError::new(StatusCode::FORBIDDEN, "cross_origin", "Cross-origin sign-in is not allowed.").into_response();
    }

    if let Some(token) = token {
        match core_auth::authenticate(&engine, &token).await {
            Ok(Some(principal)) => {
                if via == Via::Cookie && unsafe_method(&method) && !forwarded && !same_origin(request.headers()) {
                    return ApiError::new(
                        StatusCode::FORBIDDEN,
                        "cross_origin",
                        "This request came from another site. Cookie-authenticated changes must come from the HexDB admin UI.",
                    )
                    .into_response();
                }
                let login = principal.login.clone();
                let caller = std::sync::Arc::new(principal.clone());
                let trigger = principal.trigger.clone();
                request.extensions_mut().insert(principal);
                // The engine applies this caller's row filters and field masks
                // (and a trigger script's writes don't fire triggers).
                let run = hexdb_core::access::as_caller(caller, next.run(request));
                let mut response = match trigger {
                    Some(name) => hexdb_core::access::as_trigger(name, run).await,
                    None => run.await,
                };
                response.extensions_mut().insert(crate::routes::AuditUser(login));
                return response;
            }
            Ok(None) => {
                if !is_public(&method, &path) {
                    let mut response =
                        ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "The session has expired or the credentials are invalid. Sign in again.")
                            .into_response();
                    if via == Via::Cookie {
                        response.headers_mut().insert(header::SET_COOKIE, clear_cookie(&engine));
                    }
                    return response;
                }
            }
            Err(e) => return ApiError::from(e).into_response(),
        }
    } else if !is_public(&method, &path) {
        let mut response = ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "Sign in, or send an API key.").into_response();
        // With OAuth on, clients such as MCP's find the provider through the resource metadata.
        let challenge = match hexdb_core::oauth::Settings::from_engine(&engine) {
            Some(_) => format!("Bearer realm=\"HexDB\", resource_metadata=\"{}{}\"", base_url(&engine, &request), PROTECTED_RESOURCE_PATH),
            None => "Bearer realm=\"HexDB\"".to_string(),
        };
        if let Ok(value) = HeaderValue::from_str(&challenge) {
            response.headers_mut().insert(header::WWW_AUTHENTICATE, value);
        }
        return response;
    }
    next.run(request).await
}

/// Where the OAuth protected resource metadata is served.
const PROTECTED_RESOURCE_PATH: &str = "/.well-known/oauth-protected-resource";

/// This hex's address as the client reached it: `http(s)://host[:port]`.
pub fn base_url(engine: &HexDBEngine, request: &Request) -> String {
    let https = request.extensions().get::<crate::client::ViaHttps>().is_some_and(|h| h.0);
    let host = request.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or(&engine.config.network.api_endpoint);
    format!("{}://{}", if https { "https" } else { "http" }, host)
}

/// `GET /.well-known/oauth-protected-resource` (RFC 9728): the OpenID Connect
/// provider whose access tokens this lattice accepts (404 while OAuth is off).
pub async fn protected_resource(State(engine): Engine, request: Request) -> ApiResult {
    let Some(settings) = hexdb_core::oauth::Settings::from_engine(&engine) else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found", "OAuth is off on this lattice."));
    };
    Ok(Json(json!({
        "resource": base_url(&engine, &request),
        "authorization_servers": [settings.issuer],
        "bearer_methods_supported": ["header"],
        "resource_name": "HexDB",
        "resource_documentation": "https://github.com/dreaminhex/hexdb/blob/main/MANUAL.md#oauth-access-tokens",
    }))
    .into_response())
}

/// Security headers on every response.
pub async fn security_headers(State(engine): State<Arc<HexDBEngine>>, request: Request, next: Next) -> Response {
    let ui = crate::routes::is_ui_path(request.uri().path()) || request.uri().path() == "/";
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    let set = |headers: &mut HeaderMap, name: &'static str, value: &'static str| {
        headers.insert(name, HeaderValue::from_static(value));
    };
    set(headers, "x-content-type-options", "nosniff");
    set(headers, "x-frame-options", "DENY");
    set(headers, "referrer-policy", "no-referrer");
    set(headers, "cross-origin-opener-policy", "same-origin");
    set(headers, "cross-origin-resource-policy", "same-origin");
    set(headers, "permissions-policy", "camera=(), microphone=(), geolocation=(), payment=()");
    if ui {
        // The admin UI loads nothing from other origins.
        set(
            headers,
            "content-security-policy",
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        );
    } else {
        set(headers, "content-security-policy", "default-src 'none'; frame-ancestors 'none'");
        if !headers.contains_key(header::CACHE_CONTROL) {
            set(headers, "cache-control", "no-store");
        }
    }
    if engine.config.tls.enabled() {
        set(headers, "strict-transport-security", "max-age=31536000");
    }
    response
}

// ---------------------------------------------------------------------------
// /auth endpoints
// ---------------------------------------------------------------------------

fn session_cookie_header(engine: &HexDBEngine, token: &str, max_age: i64) -> HeaderValue {
    let secure = if engine.config.tls.enabled() { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        SESSION_COOKIE,
        token,
        max_age.max(0),
        secure
    ))
    .unwrap_or_else(|_| HeaderValue::from_static(""))
}

fn clear_cookie(engine: &HexDBEngine) -> HeaderValue {
    session_cookie_header(engine, "", 0)
}

fn me_json(principal: &Principal) -> serde_json::Value {
    json!({
        "user_id": principal.user_id,
        "login": principal.login,
        "email_address": principal.email_address,
        "roles": principal.roles,
        "grants": principal.grants,
        "permissions": principal.global_permissions(),
        "is_admin": principal.is_admin(),
        "credential": principal.credential,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginRequest {
    pub login: String,
    pub password: String,
    /// With MFA on: the current code from the authenticator app, or a backup code.
    #[serde(default)]
    pub code: Option<String>,
    /// Also return the token in the body (for scripts). Browsers should rely
    /// on the HttpOnly cookie so page scripts never see the token.
    #[serde(default)]
    pub return_token: bool,
}

/// Sign in: `{"login", "password"}`. Sets the session cookie; with
/// `"return_token": true` the token is also in the body.
pub async fn login(
    State(engine): Engine,
    crate::client::ClientIp(client): crate::client::ClientIp,
    body: Result<Json<LoginRequest>, axum::extract::rejection::JsonRejection>,
) -> ApiResult {
    let Json(input) = body?;
    let signed_in = core_auth::login(&engine, &input.login, &input.password, input.code.as_deref(), &client).await?;
    let max_age = signed_in.claims.exp - chrono::Utc::now().timestamp();
    let mut body = json!({
        "user": me_json(&signed_in.principal),
        "expires_at": signed_in.claims.exp,
    });
    if input.return_token {
        body["token"] = json!(signed_in.token);
    }
    let mut response = Json(body).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, session_cookie_header(&engine, &signed_in.token, max_age));
    Ok(response)
}

/// Sign out: revokes this session (a no-op for API keys) and clears the cookie.
pub async fn logout(State(engine): Engine, Auth(principal): Auth, crate::client::ClientIp(client): crate::client::ClientIp) -> ApiResult {
    if let core_auth::Credential::Session { session_id, expires_at } = &principal.credential {
        if engine.is_writable() {
            core_auth::revoke_session(&engine, session_id, *expires_at).await?;
        } else {
            crate::lattice::forward_revocation(&engine, session_id, *expires_at).await?;
        }
        info!("🔒 '{}' signed out.", principal.login);
        engine.record_audit(AuditEvent::new(&principal.login, "auth.logout", &principal.login).client(&client)).await;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(header::SET_COOKIE, clear_cookie(&engine));
    Ok(response)
}

/// The signed-in user.
pub async fn me(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    let mut me = me_json(&principal);
    // The admin UI shows demo-mode pages (read-only) to everyone.
    me["demo_mode"] = json!(engine.config.security.demo_mode);
    Ok(Json(me).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordChange {
    pub current_password: String,
    pub new_password: String,
}

/// `GET /auth/quota`: your upload quota and what you use of it.
pub async fn quota(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    Ok(Json(engine.quota_view(&principal.login).await?).into_response())
}

/// Run something that re-checks the caller's password or code, under the
/// sign-in lockout: refused while locked out, and a wrong answer (403) counts
/// as a failed attempt.
async fn reauth<T>(
    engine: &HexDBEngine,
    principal: &Principal,
    client: &str,
    action: &str,
    check: impl std::future::Future<Output = anyhow::Result<T>>,
) -> Result<T, ApiError> {
    core_auth::check_reauth(engine, &principal.login, client).await?;
    match check.await {
        Err(e) if matches!(e.downcast_ref::<hexdb_core::EngineError>(), Some(hexdb_core::EngineError::Forbidden(_))) => {
            core_auth::reauth_failed(engine, &principal.login, client, action).await;
            Err(e.into())
        }
        other => other.map_err(ApiError::from),
    }
}

/// Change your own password. Signs out every session, then starts a new one
/// for this request's client.
/// On a public demo (`security.demo_mode`) visitors share one account: none
/// of them may change its password or MFA, which would lock the others out
/// until the demo resets. Administrators can.
fn demo_account_locked(engine: &HexDBEngine, principal: &Principal) -> Result<(), ApiError> {
    if engine.config.security.demo_mode && !principal.is_admin() {
        return Err(ApiError::forbidden("This is a shared demo account: its password and MFA can't be changed."));
    }
    Ok(())
}

pub async fn change_password(
    State(engine): Engine,
    Auth(principal): Auth,
    crate::client::ClientIp(client): crate::client::ClientIp,
    body: Result<Json<PasswordChange>, axum::extract::rejection::JsonRejection>,
) -> ApiResult {
    demo_account_locked(&engine, &principal)?;
    let Json(input) = body?;
    reauth(&engine, &principal, &client, "auth.password_change", users::change_own_password(&engine, &principal.user_id, &input.current_password, &input.new_password)).await?;
    // Every older session is now invalid; issue a fresh one if this was a session.
    let mut response = StatusCode::NO_CONTENT.into_response();
    if matches!(principal.credential, core_auth::Credential::Session { .. }) {
        // The new session must be issued strictly after the cut-off (milliseconds).
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let (token, claims) = core_auth::issue_session(&engine.config.session_key()?, &principal.user_id, engine.live().session_hours);
        let max_age = claims.exp - chrono::Utc::now().timestamp();
        response.headers_mut().insert(header::SET_COOKIE, session_cookie_header(&engine, &token, max_age));
        info!(%client, "🔑 '{}' changed their password.", principal.login);
    }
    engine.record_audit(AuditEvent::new(&principal.login, "auth.password_change", &principal.login).client(&client)).await;
    Ok(response)
}

// ---------------------------------------------------------------------------
// Multi-factor authentication (see hexdb_core::mfa)
// ---------------------------------------------------------------------------

/// Whether MFA is on for the signed-in user, and how many backup codes are left.
pub async fn mfa_status(State(engine): Engine, Auth(principal): Auth) -> ApiResult {
    let (enabled, backup_codes_left, pending) = users::mfa_status(&engine, &principal.user_id).await?;
    Ok(Json(json!({ "enabled": enabled, "backup_codes_left": backup_codes_left, "enrolling": pending })).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MfaSetup {
    pub password: String,
}

/// Start enrolling an authenticator app: returns the secret and an `otpauth://` URI.
pub async fn mfa_setup(
    State(engine): Engine,
    Auth(principal): Auth,
    crate::client::ClientIp(client): crate::client::ClientIp,
    body: Result<Json<MfaSetup>, axum::extract::rejection::JsonRejection>,
) -> ApiResult {
    demo_account_locked(&engine, &principal)?;
    let Json(input) = body?;
    let (secret, uri) = reauth(&engine, &principal, &client, "auth.mfa_setup", users::mfa_setup(&engine, &principal.user_id, &input.password)).await?;
    Ok(Json(json!({ "secret": secret, "otpauth_uri": uri })).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MfaCode {
    pub code: String,
    #[serde(default)]
    pub password: Option<String>,
}

/// Confirm enrolment with a code; returns the backup codes (shown once).
/// Other sessions are signed out, and this one gets a new cookie.
pub async fn mfa_enable(
    State(engine): Engine,
    Auth(principal): Auth,
    crate::client::ClientIp(client): crate::client::ClientIp,
    body: Result<Json<MfaCode>, axum::extract::rejection::JsonRejection>,
) -> ApiResult {
    demo_account_locked(&engine, &principal)?;
    let Json(input) = body?;
    let codes = users::mfa_enable(&engine, &principal.user_id, &input.code).await?;
    engine.record_audit(AuditEvent::new(&principal.login, "auth.mfa_enable", &principal.login).client(&client)).await;
    let mut response = Json(json!({ "backup_codes": codes })).into_response();
    if matches!(principal.credential, core_auth::Credential::Session { .. }) {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let (token, claims) = core_auth::issue_session(&engine.config.session_key()?, &principal.user_id, engine.live().session_hours);
        let max_age = claims.exp - chrono::Utc::now().timestamp();
        response.headers_mut().insert(header::SET_COOKIE, session_cookie_header(&engine, &token, max_age));
    }
    Ok(response)
}

/// Turn MFA off: `{"password", "code"}`.
pub async fn mfa_disable(
    State(engine): Engine,
    Auth(principal): Auth,
    crate::client::ClientIp(client): crate::client::ClientIp,
    body: Result<Json<MfaCode>, axum::extract::rejection::JsonRejection>,
) -> ApiResult {
    demo_account_locked(&engine, &principal)?;
    let Json(input) = body?;
    let password = input.password.ok_or_else(|| ApiError::invalid("password is required."))?;
    reauth(&engine, &principal, &client, "auth.mfa_disable", users::mfa_disable(&engine, &principal.user_id, &password, &input.code)).await?;
    engine.record_audit(AuditEvent::new(&principal.login, "auth.mfa_disable", &principal.login).client(&client)).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Replace the backup codes: `{"password", "code"}`.
pub async fn mfa_backup_codes(
    State(engine): Engine,
    Auth(principal): Auth,
    crate::client::ClientIp(client): crate::client::ClientIp,
    body: Result<Json<MfaCode>, axum::extract::rejection::JsonRejection>,
) -> ApiResult {
    demo_account_locked(&engine, &principal)?;
    let Json(input) = body?;
    let password = input.password.ok_or_else(|| ApiError::invalid("password is required."))?;
    let codes = reauth(&engine, &principal, &client, "auth.mfa_backup_codes", users::mfa_new_backup_codes(&engine, &principal.user_id, &password, &input.code)).await?;
    engine.record_audit(AuditEvent::new(&principal.login, "auth.mfa_backup_codes", &principal.login).client(&client)).await;
    Ok(Json(json!({ "backup_codes": codes })).into_response())
}

#[derive(Deserialize)]
pub struct KeyListParams {
    /// Admins: list every user's keys.
    #[serde(default)]
    pub all: bool,
}

/// API keys: your own, or (admins, `?all=true`) everyone's.
pub async fn list_keys(
    State(engine): Engine,
    Auth(principal): Auth,
    params: Result<axum::extract::Query<KeyListParams>, axum::extract::rejection::QueryRejection>,
) -> ApiResult {
    let axum::extract::Query(params) = params?;
    if params.all {
        principal.require_admin()?;
    }
    let user = (!params.all).then_some(principal.user_id.as_str());
    Ok(Json(json!({ "keys": core_auth::list_api_keys(&engine, user).await? })).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewKey {
    pub name: String,
    pub expires_in_days: Option<u64>,
}

/// Create an API key for yourself. The key is in the response only.
pub async fn create_key(
    State(engine): Engine,
    Auth(principal): Auth,
    body: Result<Json<NewKey>, axum::extract::rejection::JsonRejection>,
) -> ApiResult {
    let Json(input) = body?;
    let (key, info) = core_auth::create_api_key(&engine, &principal, &input.name, input.expires_in_days).await?;
    Ok((StatusCode::CREATED, Json(json!({ "key": key, "info": info }))).into_response())
}

/// Revoke an API key (your own, or any as an admin).
pub async fn revoke_key(State(engine): Engine, Auth(principal): Auth, Path(id): Path<String>) -> ApiResult {
    if !core_auth::revoke_api_key(&engine, &principal, &id).await? {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found", "No such API key."));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_listed_routes_are_public() {
        for (method, path) in [
            (Method::GET, "/lattice/snapshot"),
            (Method::GET, "/lattice/snapshot/orders"),
            (Method::GET, "/lattice/chunks/ab12"),
            (Method::POST, "/lattice/revoke"),
            (Method::POST, "/lattice/ai-usage"),
            (Method::POST, "/auth/login"),
            (Method::GET, "/health"),
            (Method::GET, "/_s/Ab3dE7fG"),
            (Method::GET, "/_s/Ab3dE7fG/content"),
            (Method::POST, "/_s/Ab3dE7fG/unlock"),
        ] {
            assert!(is_public(&method, path), "{} {}", method, path);
        }
        for (method, path) in [
            (Method::GET, "/lattice/count"),
            (Method::GET, "/lattice/_files/a.jpg"),
            (Method::GET, "/lattice/snapshot/orders/extra"),
            (Method::DELETE, "/lattice/chunks/ab12"),
            (Method::POST, "/lattice/snapshot"),
            (Method::GET, "/lattice/"),
            (Method::GET, "/users"),
            (Method::POST, "/health"),
            (Method::DELETE, "/_s/Ab3dE7fG"),
            (Method::POST, "/_s/Ab3dE7fG/content"),
            (Method::GET, "/_s/Ab3dE7fG/other"),
            (Method::GET, "/shares"),
        ] {
            assert!(!is_public(&method, path), "{} {}", method, path);
        }
    }
}
