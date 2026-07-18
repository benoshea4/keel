// dispatch.rs — micro-cloud phase 4: the PUBLIC data plane. /fn/* requests
// are matched against the routes table (longest prefix wins) and run through
// a fresh sandboxed function instance; phase 6 reuses invoke() for app
// backends. These routes mount AFTER the auth layer in main.rs — a
// browser-served app must reach its own backend tokenless (status.md §N.5);
// the CONTROL plane (/api/routes etc.) stays token-gated like everything
// else.
//
// SQLite on the request path runs inside spawn_blocking with its own
// connection (ext spec §E1) — the route lookup AND the invocation share one
// closure, one thread, one connection lifetime.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use keel_core::db;
use keel_core::function::{self, HttpRequest};
use keel_core::runner::EngineShared;
use keel_core::sandbox::{Outcome, Quota};

/// 10 MiB request-body cap (ext spec: buffer, never stream; 413 beyond).
const BODY_CAP: usize = 10 * 1024 * 1024;

fn json_err(status: StatusCode, body: serde_json::Value) -> Response {
    (status, axum::Json(body)).into_response()
}

/// The pieces of an incoming request a function sees, extracted BEFORE the
/// blocking closure (never hold axum types across a spawn_blocking).
pub struct RawReq {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub async fn extract_raw(req: Request) -> Result<RawReq, Response> {
    let (parts, body) = req.into_parts();
    let method = parts.method.as_str().to_string();
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().unwrap_or("").to_string();
    // Non-UTF-8 header values are skipped (ext spec Task 4.3 step 2).
    let headers = parts
        .headers
        .iter()
        .filter_map(|(n, v)| Some((n.as_str().to_string(), v.to_str().ok()?.to_string())))
        .collect();
    let body = axum::body::to_bytes(body, BODY_CAP)
        .await
        .map_err(|_| {
            json_err(
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({"error": "request body exceeds 10 MiB"}),
            )
        })?
        .to_vec();
    Ok(RawReq {
        method,
        path,
        query,
        headers,
        body,
    })
}

/// Run one request through a handler component and translate the classified
/// outcome to HTTP: ok relays the guest response (content-length dropped —
/// axum recomputes it); every other outcome is a 500 naming the outcome, and
/// an engine error (db/compile) is a 500 naming the error. Shared by /fn/*
/// and the phase-6 app backend path.
pub fn run_function(
    shared: &Arc<EngineShared>,
    kind: &str,
    refname: &str,
    module_hash: &str,
    quota: Quota,
    raw: RawReq,
    guest_path: String,
) -> Response {
    let wreq = HttpRequest {
        method: raw.method,
        path: guest_path,
        query: raw.query,
        headers: raw.headers,
        body: raw.body,
    };
    match function::invoke_handler(shared, kind, refname, module_hash, quota, wreq) {
        Err(e) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": format!("{e:#}")}),
        ),
        Ok(inv) => match (inv.outcome, inv.response) {
            (Outcome::Ok, Some(resp)) => {
                let status =
                    StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                let mut out = Response::builder().status(status);
                if let Some(h) = out.headers_mut() {
                    for (name, value) in &resp.headers {
                        if name.eq_ignore_ascii_case("content-length") {
                            continue; // axum sets it from the actual body
                        }
                        // A guest emitting an invalid header name/value loses
                        // that header, not the response.
                        if let (Ok(n), Ok(v)) = (
                            HeaderName::from_bytes(name.as_bytes()),
                            HeaderValue::from_str(value),
                        ) {
                            h.insert(n, v);
                        }
                    }
                }
                out.body(Body::from(resp.body))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
            }
            (outcome, _) => json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"outcome": outcome.as_str()}),
            ),
        },
    }
}

/// GET/POST/... /fn/* — the phase-4 dispatcher (ext spec Task 4.3).
pub async fn dispatch_fn(State(shared): State<Arc<EngineShared>>, req: Request) -> Response {
    let raw = match extract_raw(req).await {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let res = tokio::task::spawn_blocking(move || {
        let conn = match db::open_conn(&shared.db_path) {
            Ok(c) => c,
            Err(e) => {
                return json_err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": format!("{e:#}")}),
                )
            }
        };
        let routes = match db::list_routes(&conn) {
            Ok(r) => r,
            Err(e) => {
                return json_err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": format!("{e:#}")}),
                )
            }
        };
        drop(conn); // invoke_handler opens its own (one per invocation, §E1)
        // Longest-prefix match ON SEGMENT BOUNDARIES: /fn/echo matches
        // /fn/echo and /fn/echo/..., never /fn/echo2 — so the guest path is
        // always "" or "/..." and the spec's "ensure leading /" case is
        // vacuous rather than surprising. N is small; no cleverness.
        let matched = routes
            .iter()
            .filter(|r| {
                raw.path == r.prefix
                    || (raw.path.starts_with(&r.prefix)
                        && raw.path.as_bytes().get(r.prefix.len()) == Some(&b'/'))
            })
            .max_by_key(|r| r.prefix.len());
        let Some(route) = matched else {
            return json_err(
                StatusCode::NOT_FOUND,
                json!({"error": format!("no route matches {}", raw.path)}),
            );
        };
        let guest_path = {
            let rest = &raw.path[route.prefix.len()..];
            if rest.is_empty() {
                "/".to_string()
            } else {
                rest.to_string()
            }
        };
        let quota = Quota {
            fuel: route.fuel_limit as u64,
            mem: route.mem_limit as usize,
            time_ms: route.time_limit_ms as u64,
        };
        let prefix = route.prefix.clone();
        let hash = route.module_hash.clone();
        run_function(&shared, "function", &prefix, &hash, quota, raw, guest_path)
    })
    .await;
    res.unwrap_or_else(|e| {
        json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": format!("function task panicked: {e}")}),
        )
    })
}
