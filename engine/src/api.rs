// api.rs — Task 1.5: the JSON API. Plain serde_json values, no bespoke DTO types
// (spec: "Plain serde_json maps; no extra types"). Errors: 404 unknown id/hash,
// 400 malformed/missing fields (axum's Json extractor already rejects syntactically
// bad JSON with 400 before our handlers run).
//
// DB access pattern: open a fresh Connection per request via db::open_conn and do
// the sub-millisecond SQLite work inline. Deliberate simplicity at hobby scale.
// RULE: never hold a Connection across an .await (it would also make the handler
// future !Send). Collect async inputs first (extractors), then do sync DB work.
//
// PHASE 2 (Task 2.5) adds POST /api/workflows/{id}/events here (+ notifier.notify).
// PHASE 3 (Task 3.6) adds POST /api/workflows/{id}/upgrade.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::db;
use crate::runner::{self, EngineShared};

type ApiErr = (StatusCode, String);

fn internal(e: anyhow::Error) -> ApiErr {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// POST /api/modules?name=demo — body = raw wasm bytes → {"hash": "<sha256hex>"}.
/// The 64MB DefaultBodyLimit lives on the route in main.rs (axum's ~2MB default
/// rejects real components).
pub async fn upload_module(
    State(shared): State<Arc<EngineShared>>,
    Query(q): Query<HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiErr> {
    let name = q.get("name").cloned().unwrap_or_default();
    let hash = hex::encode(Sha256::digest(&body)); // lowercase hex, content address
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    db::insert_module(&conn, &hash, &name, &body).map_err(internal)?;
    Ok(Json(json!({ "hash": hash })))
}

/// POST /api/workflows — {"module_hash": "...", "input": <any json>} → {"id": "..."}
pub async fn create_workflow(
    State(shared): State<Arc<EngineShared>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiErr> {
    let module_hash = body
        .get("module_hash")
        .and_then(Value::as_str)
        .ok_or((StatusCode::BAD_REQUEST, "missing module_hash".to_string()))?;
    let input = body
        .get("input")
        .ok_or((StatusCode::BAD_REQUEST, "missing input".to_string()))?;

    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    if !db::module_exists(&conn, module_hash).map_err(internal)? {
        return Err((StatusCode::NOT_FOUND, "unknown module hash".to_string()));
    }
    let id = uuid::Uuid::new_v4().to_string();
    // Input is stored as an opaque JSON string; the engine never inspects it.
    db::create_workflow(&conn, &id, module_hash, &input.to_string()).map_err(internal)?;
    runner::spawn(shared.clone(), id.clone()); // sanctioned spawn call-site 1 of 3 (§0)
    Ok(Json(json!({ "id": id })))
}

/// GET /api/workflows/{id}
pub async fn get_workflow(
    State(shared): State<Arc<EngineShared>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let wf = db::get_workflow(&conn, &id)
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "unknown workflow id".to_string()))?;
    // NOTE: output is returned as a JSON *string* (or null), not re-parsed — the
    // acceptance script knowingly checks the DB, not this field (SPEC.md Task 1.7).
    Ok(Json(json!({
        "id": wf.id,
        "status": wf.status,
        "output": wf.output,
        "module_hash": wf.module_hash,
        "created_at": wf.created_at,
        "updated_at": wf.updated_at,
    })))
}

/// GET /api/workflows/{id}/journal
pub async fn get_journal(
    State(shared): State<Arc<EngineShared>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    if db::get_workflow(&conn, &id).map_err(internal)?.is_none() {
        return Err((StatusCode::NOT_FOUND, "unknown workflow id".to_string()));
    }
    let rows = db::journal_rows(&conn, &id).map_err(internal)?;
    let out: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "seq": r.seq,
                "kind": r.kind,
                "request": r.request,     // raw JSON string, as stored (§4.2)
                "response": r.response,   // raw JSON string, as stored (§4.2)
                "created_at": r.created_at,
            })
        })
        .collect();
    Ok(Json(Value::Array(out)))
}
