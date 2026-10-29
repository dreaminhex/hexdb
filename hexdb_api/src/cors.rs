// HexDB API: cross-origin requests (CORS)
//
// Browsers let a page read answers from another origin only when that origin
// says so. `cors.allowed_origins` (lattice configuration) lists the pages that
// may: each listed origin gets `Access-Control-Allow-Origin` with its own
// value, and `*` lets any page in. Preflights (OPTIONS with
// Access-Control-Request-Method) are answered here, before authentication,
// since browsers send them without credentials.
//
// Credentials are never allowed across origins (no
// Access-Control-Allow-Credentials), so a page on another origin can't use the
// visitor's session cookie: it signs in with an API key or a bearer token it
// holds. Cookie-authenticated changes still have to come from this server's
// own pages (see `crate::auth`).

use crate::handlers::ApiError;
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use hexdb_core::engine::HexDBEngine;
use std::sync::Arc;

const ALLOW_METHODS: &str = "GET, HEAD, POST, PUT, PATCH, DELETE";
/// Response headers a page may read besides the CORS-safelisted ones.
const EXPOSE_HEADERS: &str =
    "etag, location, retry-after, content-disposition, content-range, accept-ranges, last-modified, idempotent-replayed, upload-offset, x-hexdb-version, x-hexdb-unchanged, x-hexdb-forwarded-to";
/// How long a browser may reuse a preflight's answer.
const MAX_AGE_SECONDS: &str = "600";

/// Which origins may call the API: none, any, or these (normalized).
#[derive(Debug, PartialEq)]
pub enum AllowedOrigins {
    None,
    Any,
    Listed(Vec<String>),
}

impl AllowedOrigins {
    pub fn from_engine(engine: &HexDBEngine) -> Self {
        Self::parse(engine.config_value("cors.allowed_origins").as_str().unwrap_or_default())
    }

    /// The stored setting: `*`, or origins separated by commas (already normalized).
    pub fn parse(setting: &str) -> Self {
        let origins: Vec<String> = setting.split(',').map(str::trim).filter(|o| !o.is_empty()).map(String::from).collect();
        if origins.iter().any(|o| o == "*") {
            AllowedOrigins::Any
        } else if origins.is_empty() {
            AllowedOrigins::None
        } else {
            AllowedOrigins::Listed(origins)
        }
    }

    /// The Access-Control-Allow-Origin value for a request from `origin`, if it may.
    pub fn allow(&self, origin: &str) -> Option<HeaderValue> {
        match self {
            AllowedOrigins::None => None,
            AllowedOrigins::Any => Some(HeaderValue::from_static("*")),
            AllowedOrigins::Listed(list) => {
                let origin = hexdb_core::lattice_config::normalize_origin(origin)?;
                list.contains(&origin).then(|| HeaderValue::from_str(&origin).ok()).flatten()
            }
        }
    }

    /// True if `origin` is listed by name (not just allowed by `*`).
    pub fn lists(&self, origin: &str) -> bool {
        matches!(self, AllowedOrigins::Listed(list) if hexdb_core::lattice_config::normalize_origin(origin).is_some_and(|o| list.contains(&o)))
    }
}

fn origin(headers: &HeaderMap) -> Option<String> {
    headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()).map(String::from)
}

/// Requested headers echoed back in a preflight, if they're plain header names.
fn requested_headers(headers: &HeaderMap) -> Option<HeaderValue> {
    let value = headers.get(header::ACCESS_CONTROL_REQUEST_HEADERS)?.to_str().ok()?;
    let names: Vec<&str> = value.split(',').map(str::trim).filter(|n| !n.is_empty()).collect();
    let valid = names.iter().all(|n| n.len() <= 64 && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    (valid && !names.is_empty() && names.len() <= 32).then(|| HeaderValue::from_str(&names.join(", ")).ok()).flatten()
}

fn add_vary(headers: &mut HeaderMap, value: &'static str) {
    headers.append(header::VARY, HeaderValue::from_static(value));
}

/// Answer preflights and mark answers to allowed origins.
pub async fn cors(State(engine): State<Arc<HexDBEngine>>, request: Request, next: Next) -> Response {
    let Some(origin) = origin(request.headers()).filter(|_| !crate::auth::same_origin(request.headers())) else {
        return next.run(request).await;
    };
    let allowed = AllowedOrigins::from_engine(&engine);
    let allow = allowed.allow(&origin);
    let preflight = request.method() == Method::OPTIONS && request.headers().contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);

    if preflight {
        let Some(allow) = allow else {
            return ApiError::new(StatusCode::FORBIDDEN, "cross_origin", "This origin may not call the API. An administrator can add it to cors.allowed_origins.").into_response();
        };
        let mut response = StatusCode::NO_CONTENT.into_response();
        let headers = response.headers_mut();
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, allow);
        headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static(ALLOW_METHODS));
        if let Some(requested) = requested_headers(request.headers()) {
            headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested);
        }
        headers.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static(MAX_AGE_SECONDS));
        add_vary(headers, "origin");
        add_vary(headers, "access-control-request-method");
        add_vary(headers, "access-control-request-headers");
        return response;
    }

    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    if let Some(allow) = allow {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, allow);
        headers.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static(EXPOSE_HEADERS));
    }
    // Caches must not serve one origin's answer to another.
    if !matches!(allowed, AllowedOrigins::None | AllowedOrigins::Any) {
        add_vary(headers, "origin");
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_listed_origins_are_allowed() {
        assert_eq!(AllowedOrigins::parse(""), AllowedOrigins::None);
        assert_eq!(AllowedOrigins::parse("*").allow("https://evil.example").unwrap(), "*");
        let listed = AllowedOrigins::parse("https://app.example.com, http://localhost:5173");
        assert_eq!(listed.allow("https://APP.example.com").unwrap(), "https://app.example.com");
        assert_eq!(listed.allow("https://app.example.com:443").unwrap(), "https://app.example.com");
        assert!(listed.allow("https://app.example.com.evil.example").is_none());
        assert!(listed.allow("http://app.example.com").is_none(), "the scheme is part of the origin");
        assert!(listed.allow("null").is_none());
        assert!(listed.lists("http://localhost:5173"));
        assert!(!AllowedOrigins::parse("*").lists("http://localhost:5173"), "* isn't a listing");
    }

    #[test]
    fn preflights_echo_only_plain_header_names() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCESS_CONTROL_REQUEST_HEADERS, HeaderValue::from_static("Authorization, content-type,idempotency-key"));
        assert_eq!(requested_headers(&headers).unwrap(), "Authorization, content-type, idempotency-key");
        headers.insert(header::ACCESS_CONTROL_REQUEST_HEADERS, HeaderValue::from_static("x-a; evil"));
        assert!(requested_headers(&headers).is_none());
    }
}
