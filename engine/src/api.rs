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
// The API surface is complete for all 3 phases: modules, workflows, events,
// journal, upgrade.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Form, FromRequest, Multipart, Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::Json;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::db;
use crate::runner::{self, EngineShared};

type ApiErr = (StatusCode, String);

fn internal(e: anyhow::Error) -> ApiErr {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn bad(e: impl std::fmt::Display) -> ApiErr {
    (StatusCode::BAD_REQUEST, e.to_string())
}

fn content_type(req: &Request) -> String {
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// POST /api/modules — two body shapes (Task 2.8) → {"hash": "<sha256hex>"}:
///   * raw wasm bytes (scripts, acceptance): POST /api/modules?name=demo
///   * multipart form (the modules page): fields `file` (wasm) + `name`
/// The 64MB DefaultBodyLimit lives on the route in main.rs (axum's ~2MB default
/// rejects real components) and covers both shapes.
pub async fn upload_module(
    State(shared): State<Arc<EngineShared>>,
    Query(q): Query<HashMap<String, String>>,
    req: Request,
) -> Result<Json<Value>, ApiErr> {
    let mut name = q.get("name").cloned().unwrap_or_default();
    let wasm: Bytes = if content_type(&req).starts_with("multipart/form-data") {
        let mut mp = Multipart::from_request(req, &()).await.map_err(bad)?;
        let mut file = None;
        while let Some(field) = mp.next_field().await.map_err(bad)? {
            match field.name() {
                Some("file") => file = Some(field.bytes().await.map_err(bad)?),
                Some("name") => name = field.text().await.map_err(bad)?,
                _ => {}
            }
        }
        file.ok_or((
            StatusCode::BAD_REQUEST,
            "missing file field — attach the .wasm component".to_string(),
        ))?
    } else {
        Bytes::from_request(req, &()).await.map_err(bad)?
    };
    let hash = hex::encode(Sha256::digest(&wasm)); // lowercase hex, content address
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    db::insert_module(&conn, &hash, &name, &wasm).map_err(internal)?;
    Ok(Json(json!({ "hash": hash })))
}

/// POST /api/workflows — two body shapes (Task 2.8) → {"id": "..."}:
///   * JSON (API): {"module_hash": "...", "input": <any json>}
///   * urlencoded form (the modules page): module_hash=...&input=<json text>
pub async fn create_workflow(
    State(shared): State<Arc<EngineShared>>,
    req: Request,
) -> Result<Json<Value>, ApiErr> {
    let (module_hash, input_json) = if content_type(&req)
        .starts_with("application/x-www-form-urlencoded")
    {
        let Form(f) = Form::<HashMap<String, String>>::from_request(req, &())
            .await
            .map_err(bad)?;
        let hash = f
            .get("module_hash")
            .cloned()
            .ok_or((StatusCode::BAD_REQUEST, "missing module_hash".to_string()))?;
        let text = f
            .get("input")
            .ok_or((StatusCode::BAD_REQUEST, "missing input".to_string()))?;
        // Forms carry JSON as text; validate it here so a typo'd input fails the
        // request, not the workflow.
        let v: Value = serde_json::from_str(text).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("input is not valid JSON ({e}) — send a JSON value, e.g. {{}}"),
            )
        })?;
        (hash, v.to_string())
    } else {
        let Json(body) = Json::<Value>::from_request(req, &()).await.map_err(bad)?;
        let hash = body
            .get("module_hash")
            .and_then(Value::as_str)
            .ok_or((StatusCode::BAD_REQUEST, "missing module_hash".to_string()))?
            .to_string();
        let input = body
            .get("input")
            .ok_or((StatusCode::BAD_REQUEST, "missing input".to_string()))?;
        (hash, input.to_string())
    };

    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    if !db::module_exists(&conn, &module_hash).map_err(internal)? {
        return Err((StatusCode::NOT_FOUND, "unknown module hash".to_string()));
    }
    let id = uuid::Uuid::new_v4().to_string();
    // Input is stored as an opaque JSON string; the engine never inspects it.
    db::create_workflow(&conn, &id, &module_hash, &input_json).map_err(internal)?;
    runner::spawn(shared.clone(), id.clone()); // sanctioned spawn call-site 1 of 3 (§0)
    Ok(Json(json!({ "id": id })))
}

/// POST /api/workflows/{id}/events → 202. Two body shapes (Task 2.8):
///   * JSON (API): {"name": "approve", "payload": <any json>}
///   * urlencoded form (the workflow page's "Send event" form via hx-post):
///     name=approve&payload=<json text>
/// The payload is stored as its JSON text; await-event hands that string to the
/// guest verbatim. Queueing is fire-and-forget: the workflow needn't be parked on
/// a matching await-event yet (or ever) — undelivered events simply wait.
pub async fn post_event(
    State(shared): State<Arc<EngineShared>>,
    Path(id): Path<String>,
    req: Request,
) -> Result<StatusCode, ApiErr> {
    let (name, payload_json) = if content_type(&req)
        .starts_with("application/x-www-form-urlencoded")
    {
        let Form(f) = Form::<HashMap<String, String>>::from_request(req, &())
            .await
            .map_err(bad)?;
        let name = f
            .get("name")
            .cloned()
            .ok_or((StatusCode::BAD_REQUEST, "missing name".to_string()))?;
        let text = f
            .get("payload")
            .ok_or((StatusCode::BAD_REQUEST, "missing payload".to_string()))?;
        let v: Value = serde_json::from_str(text).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!(r#"payload is not valid JSON ({e}) — send a JSON value, e.g. {{"by":"alice"}}"#),
            )
        })?;
        (name, v.to_string())
    } else {
        let Json(body) = Json::<Value>::from_request(req, &()).await.map_err(bad)?;
        let name = body
            .get("name")
            .and_then(Value::as_str)
            .ok_or((StatusCode::BAD_REQUEST, "missing name".to_string()))?
            .to_string();
        let payload = body
            .get("payload")
            .ok_or((StatusCode::BAD_REQUEST, "missing payload".to_string()))?;
        (name, payload.to_string())
    };

    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    if db::get_workflow(&conn, &id).map_err(internal)?.is_none() {
        return Err((StatusCode::NOT_FOUND, "unknown workflow id".to_string()));
    }
    db::insert_event(&conn, &id, &name, &payload_json).map_err(internal)?;
    shared.notifier.notify(&id); // wake the park loop now instead of ≤1s later
    Ok(StatusCode::ACCEPTED)
}

/// Step-1 claim on an in-flight upgrade. Drop removes the id on EVERY exit path
/// of the handler — success, validation 409, timeout, panic (SPEC.md Task 3.6).
struct UpgradeClaim {
    shared: Arc<EngineShared>,
    id: String,
}

impl UpgradeClaim {
    fn acquire(shared: &Arc<EngineShared>, id: &str) -> Option<UpgradeClaim> {
        if shared.upgrades.lock().unwrap().insert(id.to_string()) {
            Some(UpgradeClaim {
                shared: shared.clone(),
                id: id.to_string(),
            })
        } else {
            None
        }
    }
}

impl Drop for UpgradeClaim {
    fn drop(&mut self) {
        self.shared.upgrades.lock().unwrap().remove(&self.id);
    }
}

/// POST /api/workflows/{id}/upgrade — Task 3.6. Two body shapes like the other
/// write endpoints: JSON {"module_hash": "<new>"} or the detail page's form.
/// Moves a PARKED, checkpointed workflow onto new code: aborts its thread at the
/// park point, discards the journal tail beyond the checkpoint (un-delivering
/// events that tail had consumed), points workflow + snapshot at the new module,
/// and respawns — the guest resumes from its checkpoint state under new code.
pub async fn upgrade_workflow(
    State(shared): State<Arc<EngineShared>>,
    Path(id): Path<String>,
    req: Request,
) -> Result<Json<Value>, ApiErr> {
    let new_hash = if content_type(&req).starts_with("application/x-www-form-urlencoded") {
        let Form(f) = Form::<HashMap<String, String>>::from_request(req, &())
            .await
            .map_err(bad)?;
        f.get("module_hash")
            .cloned()
            .ok_or((StatusCode::BAD_REQUEST, "missing module_hash".to_string()))?
    } else {
        let Json(body) = Json::<Value>::from_request(req, &()).await.map_err(bad)?;
        body.get("module_hash")
            .and_then(Value::as_str)
            .ok_or((StatusCode::BAD_REQUEST, "missing module_hash".to_string()))?
            .to_string()
    };

    // Step 1 — claim.
    let _claim = UpgradeClaim::acquire(&shared, &id).ok_or((
        StatusCode::CONFLICT,
        "upgrade already in progress".to_string(),
    ))?;

    // Step 2 — validate (scoped connection: never held across an .await).
    {
        let conn = db::open_conn(&shared.db_path).map_err(internal)?;
        let wf = db::get_workflow(&conn, &id)
            .map_err(internal)?
            .ok_or((StatusCode::NOT_FOUND, "unknown workflow id".to_string()))?;
        if !db::module_exists(&conn, &new_hash).map_err(internal)? {
            return Err((StatusCode::NOT_FOUND, "unknown module hash".to_string()));
        }
        if wf.status != "sleeping" && wf.status != "waiting_event" {
            return Err((
                StatusCode::CONFLICT,
                format!(
                    "workflow is {} — only parked (sleeping/waiting_event) workflows can be upgraded",
                    wf.status
                ),
            ));
        }
        if db::get_snapshot(&conn, &id).map_err(internal)?.is_none() {
            return Err((
                StatusCode::CONFLICT,
                "no checkpoint yet — the guest must call checkpoint before it can be upgraded"
                    .to_string(),
            ));
        }
    }

    // Step 3 — yank the parked worker. set_abort() also notifies, so the park
    // loop re-checks immediately and bails with AbortForUpgrade (the runner exits
    // that thread silently, status untouched).
    shared.notifier.set_abort(&id);
    if let Some(h) = shared.take_thread(&id) {
        // Bounded join WITHOUT blocking the async handler: poll is_finished and
        // only join once true (then it's instant). Deviation from the spec's
        // spawn_blocking sketch (status.md dev. 14): on timeout the handle goes
        // BACK into the registry, so a retry joins THIS still-running thread
        // instead of concluding "already exited" and racing a live worker.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if h.is_finished() {
                let _ = h.join(); // instant; worker errors were already recorded
                break;
            }
            if std::time::Instant::now() >= deadline {
                // Leaving the abort flag set here would zombie the workflow at
                // its next park — clearing it is MANDATORY (spec step 3).
                shared.notifier.clear_abort(&id);
                shared.put_thread(&id, h);
                return Err((
                    StatusCode::CONFLICT,
                    "workflow is actively executing (not parked); retry when it parks"
                        .to_string(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    // The joined worker's runner cleared the flag on its way out; the
    // thread-already-exited path never observed it. Clear unconditionally so a
    // set-but-unobserved flag can't instantly abort the respawned workflow.
    shared.notifier.clear_abort(&id);

    // Steps 4+5 — re-read state post-join: the worker may have advanced (even
    // completed) between validation and the abort landing. C comes from the
    // CURRENT snapshot; a no-longer-parked workflow must not be resurrected.
    let c_seq = {
        let mut conn = db::open_conn(&shared.db_path).map_err(internal)?;
        let wf = db::get_workflow(&conn, &id)
            .map_err(internal)?
            .ok_or((StatusCode::NOT_FOUND, "unknown workflow id".to_string()))?;
        if wf.status != "sleeping" && wf.status != "waiting_event" {
            return Err((
                StatusCode::CONFLICT,
                format!("workflow moved to {} during the upgrade — retry", wf.status),
            ));
        }
        let snap = db::get_snapshot(&conn, &id).map_err(internal)?.ok_or((
            StatusCode::CONFLICT,
            "snapshot disappeared during the upgrade — retry".to_string(),
        ))?;
        db::upgrade_module_txn(&mut conn, &id, snap.journal_seq, &new_hash).map_err(internal)?;
        snap.journal_seq
    };
    runner::spawn(shared.clone(), id.clone()); // sanctioned spawn call-site 3 of 3 (§0)
    Ok(Json(
        json!({ "id": id, "module_hash": new_hash, "resumed_from_seq": c_seq }),
    ))
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
