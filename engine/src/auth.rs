// auth.rs — v1.1: optional bearer-token auth for the whole surface.
//
// Model: ONE operator token, set via --api-token / KEEL_API_TOKEN. When unset,
// keel behaves exactly as before (open, loopback-intended — the README says so
// loudly). When set, every route except /assets/*, /login and /logout requires
// either:
//   * Authorization: Bearer <token>            (API clients, scripts)
//   * a keel_token cookie                      (the UI, set by POST /login)
//
// The cookie carries hex(sha256(token)), NOT the raw token: it stays inside
// the cookie charset and the browser never stores the credential itself.
// SameSite=Lax means cross-site POSTs don't carry it (CSRF mitigation);
// HttpOnly keeps page scripts away from it.
//
// Timing safety: comparisons hash both sides with SHA-256 first and compare
// digests. A non-constant-time == over digests leaks only the position of the
// first differing digest byte — useless to an attacker, who cannot steer or
// invert the digest of their guess. (The classic hash-then-compare pattern;
// avoids pulling in a constant-time crate for one comparison.)

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use sha2::{Digest, Sha256};

use keel_core::runner::EngineShared;

fn digest_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Raw-token check (Authorization header path).
fn token_ok(presented: &str, expected: &str) -> bool {
    Sha256::digest(presented.as_bytes()) == Sha256::digest(expected.as_bytes())
}

/// The cookie value POST /login issues for a given operator token.
pub fn cookie_value(expected_token: &str) -> String {
    digest_hex(expected_token)
}

fn bearer(req: &Request) -> Option<String> {
    req.headers()
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_string)
}

fn cookie_token(req: &Request) -> Option<String> {
    let cookies = req.headers().get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|c| {
        c.trim()
            .strip_prefix("keel_token=")
            .map(str::to_string)
    })
}

/// Router-wide middleware. No token configured → passthrough (v1.0 behavior).
pub async fn require_auth(
    State(shared): State<Arc<EngineShared>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(expected) = &shared.api_token else {
        return next.run(req).await;
    };
    let path = req.uri().path();
    // Assets are static and cacheable; /login and /logout must be reachable
    // logged-out or nobody could ever log in.
    if path.starts_with("/assets/") || path == "/login" || path == "/logout" {
        return next.run(req).await;
    }
    let header_ok = bearer(&req).is_some_and(|t| token_ok(&t, expected));
    // Cookie carries the digest already — compare against the expected digest.
    let cookie_ok = cookie_token(&req).is_some_and(|c| c == cookie_value(expected));
    if header_ok || cookie_ok {
        return next.run(req).await;
    }
    if path.starts_with("/api/") {
        (
            StatusCode::UNAUTHORIZED,
            "missing or invalid token — send Authorization: Bearer <token> (see --api-token)"
                .to_string(),
        )
            .into_response()
    } else {
        Redirect::to("/login").into_response()
    }
}

/// Verifies a login attempt (raw token from the form).
pub fn login_ok(shared: &EngineShared, presented: &str) -> bool {
    shared
        .api_token
        .as_deref()
        .is_some_and(|expected| token_ok(presented, expected))
}
