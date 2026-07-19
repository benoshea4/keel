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

use keel_core::db;
use keel_core::runner::{self, EngineShared};

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
///
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
    // Post-review hardening: reject bytes that cannot be wasm at the door. The
    // full "does it match the workflow world" check runs before anything acts on
    // the module (workflow start fails fast; the upgrade pre-flights below) —
    // this just stops arbitrary junk from earning a content hash and a 200.
    if !wasm.starts_with(b"\0asm") {
        return Err((
            StatusCode::BAD_REQUEST,
            "not a WebAssembly binary (missing \\0asm magic) — upload a component built with cargo component".to_string(),
        ));
    }
    let hash = hex::encode(Sha256::digest(&wasm)); // lowercase hex, content address
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    db::insert_module(&conn, &hash, &name, &wasm).map_err(internal)?;
    Ok(Json(json!({ "hash": hash })))
}

/// v2.6 — POST /api/providers → {"hash": "<sha256hex>"} — the LIVE provider
/// registry (PROVIDERS.md). Three shapes:
///   * raw wasm bytes: POST /api/providers?name=X&tier=pure|effectful
///   * multipart form (the providers page): fields `file`, `name`, `tier`
///   * rebind (rollback, no bytes): POST /api/providers?name=X&tier=T&hash=H
///
/// Pre-flighted for the tier at the door — a bad component is a 400 here,
/// never a workflow failure. The swap is live: the next provider-call under
/// this name uses the new component; recorded journal rows replay unchanged.
pub async fn upload_provider(
    State(shared): State<Arc<EngineShared>>,
    Query(q): Query<HashMap<String, String>>,
    req: Request,
) -> Result<Json<Value>, ApiErr> {
    let mut name = q.get("name").cloned().unwrap_or_default();
    let mut tier = q.get("tier").cloned().unwrap_or_default();
    let rebind_hash = q.get("hash").cloned();
    let wasm: Bytes = if content_type(&req).starts_with("multipart/form-data") {
        let mut mp = Multipart::from_request(req, &()).await.map_err(bad)?;
        let mut file = None;
        while let Some(field) = mp.next_field().await.map_err(bad)? {
            match field.name() {
                Some("file") => file = Some(field.bytes().await.map_err(bad)?),
                Some("name") => name = field.text().await.map_err(bad)?,
                Some("tier") => tier = field.text().await.map_err(bad)?,
                _ => {}
            }
        }
        file.unwrap_or_default()
    } else {
        Bytes::from_request(req, &()).await.map_err(bad)?
    };
    let effectful = match tier.as_str() {
        "pure" => false,
        "effectful" => true,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("tier must be 'pure' or 'effectful', got '{other}' — the tier is an operator grant, so it is required"),
            ))
        }
    };
    let eng = keel_core::Engine::from_shared(shared.clone());
    if let Some(h) = rebind_hash {
        if !wasm.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                "send either bytes or ?hash= (rebind), not both".to_string(),
            ));
        }
        return match eng.rebind_provider(&name, effectful, &h) {
            Ok(true) => Ok(Json(json!({ "hash": h }))),
            Ok(false) => Err((
                StatusCode::NOT_FOUND,
                format!("no stored provider blob with hash {h}"),
            )),
            Err(e) => Err((StatusCode::BAD_REQUEST, format!("{e:#}"))),
        };
    }
    if !wasm.starts_with(b"\0asm") {
        return Err((
            StatusCode::BAD_REQUEST,
            "not a WebAssembly binary (missing \\0asm magic) — upload a component built with cargo component".to_string(),
        ));
    }
    match eng.upload_provider(&name, effectful, &wasm) {
        Ok(hash) => Ok(Json(json!({ "hash": hash }))),
        // Pre-flight/validation failures dominate here and are the caller's
        // to fix (wrong tier, wrong world, bad name).
        Err(e) => Err((StatusCode::BAD_REQUEST, format!("{e:#}"))),
    }
}

/// v2.6 — GET /api/providers → [{name, tier, hash, updated_at}]
pub async fn list_providers(
    State(shared): State<Arc<EngineShared>>,
) -> Result<Json<Value>, ApiErr> {
    let rows = keel_core::Engine::from_shared(shared.clone())
        .list_providers()
        .map_err(internal)?;
    let out: Vec<Value> = rows
        .iter()
        .map(|(name, effectful, hash, updated_at)| {
            json!({
                "name": name,
                "tier": if *effectful { "effectful" } else { "pure" },
                "hash": hash,
                "updated_at": updated_at,
            })
        })
        .collect();
    Ok(Json(Value::Array(out)))
}

/// v2.6 — DELETE /api/providers/{name} — unbind (the blob stays for rebind);
/// subsequent calls to the name err as unregistered, journaled as data.
pub async fn delete_provider(
    State(shared): State<Arc<EngineShared>>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiErr> {
    if keel_core::Engine::from_shared(shared.clone())
        .remove_provider(&name)
        .map_err(internal)?
    {
        Ok(Json(json!({ "deleted": name })))
    } else {
        Err((StatusCode::NOT_FOUND, format!("no provider '{name}'")))
    }
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
    drop(conn);
    // v2.2 — the HTTP path and embedders share ONE create+spawn implementation
    // (spawn call-site 1 of 4 lives inside start_workflow, lib.rs).
    let id = keel_core::Engine::from_shared(shared.clone())
        .start_workflow(&module_hash, &input_json)
        .map_err(internal)?;
    Ok(Json(json!({ "id": id })))
}

/// POST /api/workflows/{id}/events → 202. Two body shapes (Task 2.8):
///   * JSON (API): {"name": "approve", "payload": <any json>}
///   * urlencoded form (the workflow page's "Send event" form via hx-post):
///     name=approve&payload=<json text>
///
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

/// Step-1 claim on an in-flight upgrade OR cancel (one per-workflow operation at
/// a time — they must not interleave). Drop removes the id on EVERY exit path of
/// the handler — success, validation 409, timeout, panic (SPEC.md Task 3.6).
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

/// Shared by upgrade step 3 and the cancel endpoint: raise the abort flag, then
/// join the workflow's thread bounded (30s, polling is_finished — never a
/// blocking join). On timeout the flag is cleared and the handle goes BACK so a
/// retry joins THIS still-running thread (status.md dev. 14). On success the
/// flag is cleared unconditionally: the thread-already-exited path never
/// observed it, and a set-but-unobserved flag would instantly abort the next
/// spawn of this workflow (status.md dev. 15).
async fn abort_and_join(shared: &Arc<EngineShared>, id: &str) -> Result<(), ApiErr> {
    shared.notifier.set_abort(id);
    if let Some(h) = shared.take_thread(id) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if h.is_finished() {
                let _ = h.join(); // instant; worker outcomes were already recorded
                break;
            }
            if std::time::Instant::now() >= deadline {
                shared.notifier.clear_abort(id);
                shared.put_thread(id, h);
                return Err((
                    StatusCode::CONFLICT,
                    "workflow is busy executing (a host call is in flight); retry shortly"
                        .to_string(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    shared.notifier.clear_abort(id);
    Ok(())
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
        let wasm = db::get_module_wasm(&conn, &new_hash)
            .map_err(internal)?
            .ok_or((StatusCode::NOT_FOUND, "unknown module hash".to_string()))?;
        // Post-review hardening: pre-flight the NEW module before anything
        // destructive. Without this, upgrading to a module that can't compile or
        // doesn't export the workflow world discarded the journal tail and THEN
        // failed at respawn — bricking the workflow (failed is terminal). Sync
        // CPU work in an async handler, accepted: upgrades are rare, and the
        // compile is a cache hit for any module that has ever run.
        runner::preflight(&shared, &new_hash, &wasm).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("new module failed pre-flight (upgrading to it would brick the workflow): {e:#}"),
            )
        })?;
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
    // loop re-checks immediately and bails with AbortForUpgrade (the runner
    // exits that thread silently, status untouched).
    abort_and_join(&shared, &id).await?;

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
    runner::spawn(shared.clone(), id.clone()); // sanctioned spawn call-site 3 of 4 (§0)
    Ok(Json(
        json!({ "id": id, "module_hash": new_hash, "resumed_from_seq": c_seq }),
    ))
}

/// POST /api/workflows/{id}/cancel — post-review hardening: the operator's off
/// switch. Two paths converge on the same abort flag: parked workflows bail in
/// their park loop immediately (set_abort notifies), and guests spinning in
/// pure wasm trap at the next epoch tick (≤1s — runner.rs's deadline callback).
/// The workflow lands in 'failed' with an explanatory output; terminal states
/// are permanent, so completed/failed workflows refuse with 409. No body.
pub async fn cancel_workflow(
    State(shared): State<Arc<EngineShared>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiErr> {
    // Same claim as upgrade: a cancel and an upgrade must not interleave.
    let _claim = UpgradeClaim::acquire(&shared, &id).ok_or((
        StatusCode::CONFLICT,
        "another operation (upgrade or cancel) is in progress for this workflow".to_string(),
    ))?;

    {
        let conn = db::open_conn(&shared.db_path).map_err(internal)?;
        let wf = db::get_workflow(&conn, &id)
            .map_err(internal)?
            .ok_or((StatusCode::NOT_FOUND, "unknown workflow id".to_string()))?;
        if wf.status == "completed" || wf.status == "failed" {
            return Err((
                StatusCode::CONFLICT,
                format!("workflow is already {} — nothing to cancel", wf.status),
            ));
        }
    }

    abort_and_join(&shared, &id).await?;

    // The worker is gone, the claim blocks upgrade, and nothing else spawns
    // existing ids mid-run (creation mints fresh uuids; recovery is startup-
    // only). Re-check before the terminal write: the worker may have finished
    // on its own between validation and the abort landing — outcomes stand.
    let mut conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let wf = db::get_workflow(&conn, &id)
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "unknown workflow id".to_string()))?;
    if wf.status == "completed" || wf.status == "failed" {
        return Err((
            StatusCode::CONFLICT,
            format!("workflow reached {} before the cancel landed", wf.status),
        ));
    }
    db::finish_cancel(&mut conn, &id, "cancelled by operator").map_err(internal)?;
    Ok(Json(
        json!({ "id": id, "status": "failed", "output": "cancelled by operator" }),
    ))
}

/// GET /api/workflows?status=&limit=&offset= — v1.3 paged listing, newest
/// first. Returns row metadata (not input/output blobs — fetch a workflow by
/// id for those). limit caps at 500.
pub async fn list_workflows(
    State(shared): State<Arc<EngineShared>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiErr> {
    let status = q.get("status").map(String::as_str);
    if let Some(s) = status {
        const KNOWN: [&str; 5] = ["running", "sleeping", "waiting_event", "completed", "failed"];
        if !KNOWN.contains(&s) {
            return Err(bad(format!("unknown status '{s}' — one of {KNOWN:?}")));
        }
    }
    let limit: i64 = q
        .get("limit")
        .map(|v| v.parse())
        .transpose()
        .map_err(bad)?
        .unwrap_or(100)
        .clamp(1, 500);
    let offset: i64 = q
        .get("offset")
        .map(|v| v.parse())
        .transpose()
        .map_err(bad)?
        .unwrap_or(0)
        .max(0);
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let rows = db::list_workflows_page(&conn, status, limit, offset).map_err(internal)?;
    let out: Vec<Value> = rows
        .iter()
        .map(|w| {
            json!({
                "id": w.id,
                "status": w.status,
                "module_hash": w.module_hash,
                "created_at": w.created_at,
                "updated_at": w.updated_at,
            })
        })
        .collect();
    Ok(Json(Value::Array(out)))
}

/// POST /api/schedules — v1.2: {"module_hash","input","interval_ms"} → a new
/// workflow every interval (first fire one interval from now; a schedule that
/// missed windows while the engine was down fires ONCE, not once per miss).
/// Floor 1000ms so a typo can't peg the engine.
/// v2.1: {"cron": "sec min hour dom mon dow"} (UTC, 6 fields) instead of
/// interval_ms — exactly one of the two, validated here so a bad expression
/// fails the request, not the scheduler thread.
/// v2.4: also accepts the schedules page's urlencoded form (interval_ms/cron
/// as text fields; empty string = absent).
pub async fn create_schedule(
    State(shared): State<Arc<EngineShared>>,
    req: Request,
) -> Result<Json<Value>, ApiErr> {
    let (hash, input, interval_ms, cron_expr): (String, Value, Option<i64>, Option<String>) =
        if content_type(&req).starts_with("application/x-www-form-urlencoded") {
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
            let input: Value = serde_json::from_str(text).map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("input is not valid JSON ({e}) — send a JSON value, e.g. {{}}"),
                )
            })?;
            let interval_ms = match f.get("interval_ms").map(String::as_str) {
                None | Some("") => None,
                Some(v) => Some(v.parse::<i64>().map_err(|_| {
                    bad(format!("interval_ms must be an integer, got '{v}'"))
                })?),
            };
            let cron = f.get("cron").filter(|c| !c.is_empty()).cloned();
            (hash, input, interval_ms, cron)
        } else {
            let Json(body) = Json::<Value>::from_request(req, &()).await.map_err(bad)?;
            let hash = body
                .get("module_hash")
                .and_then(Value::as_str)
                .ok_or((StatusCode::BAD_REQUEST, "missing module_hash".to_string()))?
                .to_string();
            let input = body
                .get("input")
                .cloned()
                .ok_or((StatusCode::BAD_REQUEST, "missing input".to_string()))?;
            let interval_ms = body.get("interval_ms").and_then(Value::as_i64);
            let cron = body
                .get("cron")
                .and_then(Value::as_str)
                .map(str::to_string);
            (hash, input, interval_ms, cron)
        };
    let (hash, input, cron_expr) = (hash.as_str(), &input, cron_expr.as_deref());

    let (interval_ms, first) = match (interval_ms, cron_expr) {
        (Some(_), Some(_)) => {
            return Err(bad("give interval_ms OR cron, not both"));
        }
        (None, None) => {
            return Err(bad("missing interval_ms (integer) or cron (6-field string)"));
        }
        (Some(ms), None) => {
            if ms < 1000 {
                return Err(bad("interval_ms must be >= 1000"));
            }
            (ms, keel_core::journal::now_ms() + ms)
        }
        (None, Some(expr)) => {
            let c = keel_core::cron::parse(expr).map_err(bad)?;
            let first = c.next_after(keel_core::journal::now_ms()).ok_or_else(|| {
                bad(format!("cron '{expr}' never matches a future time (impossible date?)"))
            })?;
            (0, first)
        }
    };
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    if !db::module_exists(&conn, hash).map_err(internal)? {
        return Err((StatusCode::NOT_FOUND, "unknown module hash".to_string()));
    }
    let id = uuid::Uuid::new_v4().to_string();
    db::insert_schedule(&conn, &id, hash, &input.to_string(), interval_ms, cron_expr, first)
        .map_err(internal)?;
    Ok(Json(json!({ "id": id, "next_run_at": first })))
}

/// PATCH /api/schedules/{id} — v2.1: {"enabled": true|false}. Pause/resume
/// firing; a re-enabled interval schedule fires once for the paused gap (the
/// collapse math), a cron schedule at its next expression match.
/// v2.4: also accepts the schedules page's urlencoded form (enabled=true|false).
pub async fn patch_schedule(
    State(shared): State<Arc<EngineShared>>,
    Path(id): Path<String>,
    req: Request,
) -> Result<Json<Value>, ApiErr> {
    let enabled = if content_type(&req).starts_with("application/x-www-form-urlencoded") {
        let Form(f) = Form::<HashMap<String, String>>::from_request(req, &())
            .await
            .map_err(bad)?;
        match f.get("enabled").map(String::as_str) {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        }
    } else {
        let Json(body) = Json::<Value>::from_request(req, &()).await.map_err(bad)?;
        body.get("enabled").and_then(Value::as_bool)
    };
    let enabled = enabled
        .ok_or((StatusCode::BAD_REQUEST, "missing enabled (boolean)".to_string()))?;
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    if db::set_schedule_enabled(&conn, &id, enabled).map_err(internal)? {
        Ok(Json(json!({ "id": id, "enabled": enabled })))
    } else {
        Err((StatusCode::NOT_FOUND, "unknown schedule id".to_string()))
    }
}

/// GET /api/schedules
pub async fn list_schedules(
    State(shared): State<Arc<EngineShared>>,
) -> Result<Json<Value>, ApiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let rows = db::list_schedules(&conn).map_err(internal)?;
    let out: Vec<Value> = rows
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "module_hash": s.module_hash,
                "input": s.input,
                "interval_ms": s.interval_ms,
                "cron": s.cron,
                "next_run_at": s.next_run_at,
                "enabled": s.enabled,
            })
        })
        .collect();
    Ok(Json(Value::Array(out)))
}

/// DELETE /api/schedules/{id} → 204. Already-created workflows are untouched.
pub async fn delete_schedule(
    State(shared): State<Arc<EngineShared>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    if db::delete_schedule(&conn, &id).map_err(internal)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, "unknown schedule id".to_string()))
    }
}

/// GET /metrics — v1.3: Prometheus text format, hand-rolled (no deps). Behind
/// the same auth as everything else; scrapers send the bearer token.
pub async fn metrics(State(shared): State<Arc<EngineShared>>) -> Result<String, ApiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let counts = db::status_counts(&conn).map_err(internal)?;
    let mut out = String::from(
        "# HELP keel_workflows Workflows by status.\n# TYPE keel_workflows gauge\n",
    );
    for (status, n) in counts {
        out.push_str(&format!("keel_workflows{{status=\"{status}\"}} {n}\n"));
    }
    out.push_str("# HELP keel_worker_threads Live workflow worker threads.\n# TYPE keel_worker_threads gauge\n");
    out.push_str(&format!("keel_worker_threads {}\n", shared.thread_count()));
    // v2.4 — threads EXECUTING (holding a --max-running permit); worker
    // threads beyond this are parked waiting for a slot. load_test.sh
    // asserts this never exceeds the cap.
    out.push_str("# HELP keel_active_permits Worker threads holding a --max-running permit.\n# TYPE keel_active_permits gauge\n");
    out.push_str(&format!("keel_active_permits {}\n", shared.active_permits()));
    // Amendment 1 (A1) — 429'd admissions since boot. Deliberately NOT a
    // ledger row (admission isn't a sandbox outcome), so it is only here.
    out.push_str("# HELP keel_fn_rate_limited_total Data-plane requests rejected 429 by a rate limit.\n# TYPE keel_fn_rate_limited_total counter\n");
    out.push_str(&format!(
        "keel_fn_rate_limited_total {}\n",
        shared.fn_rate_limited.load(std::sync::atomic::Ordering::Relaxed)
    ));
    // v3.3 (P-FIX-2/4) — the global-cap rejections and the bounded compile
    // cache, both observable so accept_harden.sh can assert them from outside.
    out.push_str("# HELP keel_fn_over_capacity_total Data-plane requests rejected 503 by --max-fn-concurrent.\n# TYPE keel_fn_over_capacity_total counter\n");
    out.push_str(&format!(
        "keel_fn_over_capacity_total {}\n",
        shared.fn_over_capacity.load(std::sync::atomic::Ordering::Relaxed)
    ));
    // v4.0 (E4) — outbound requests actually made by granted proxy refs.
    out.push_str("# HELP keel_fn_outbound_total Outbound HTTP requests made by outbound-granted refs.\n# TYPE keel_fn_outbound_total counter\n");
    out.push_str(&format!(
        "keel_fn_outbound_total {}\n",
        shared.fn_outbound.load(std::sync::atomic::Ordering::Relaxed)
    ));
    out.push_str("# HELP keel_compiled_cache_size Compiled components held in memory (bounded by --max-compiled-modules).\n# TYPE keel_compiled_cache_size gauge\n");
    out.push_str(&format!(
        "keel_compiled_cache_size {}\n",
        shared.compiled_cache_size()
    ));
    // v3.4 (R.5) — latency percentiles the ledger already contained: nearest-
    // rank over duration_ms per (kind, ref). Gauges, not histograms — the raw
    // rows ARE the histogram, re-derived per scrape.
    out.push_str("# HELP keel_fn_duration_ms Invocation duration percentiles per ref (nearest-rank from the ledger).\n# TYPE keel_fn_duration_ms gauge\n");
    for (kind, refname, p50, p95, p99) in db::duration_percentiles(&conn).map_err(internal)? {
        for (q, v) in [("0.5", p50), ("0.95", p95), ("0.99", p99)] {
            out.push_str(&format!(
                "keel_fn_duration_ms{{kind=\"{kind}\",ref=\"{refname}\",quantile=\"{q}\"}} {v}\n"
            ));
        }
    }
    Ok(out)
}

/// GET /api/logs?kind=function|app&ref=<prefix-or-name>[&after=<id>][&limit=<n>]
/// — Amendment 1 (A2). Without `after`: the newest `limit` lines, oldest-first.
/// With `after`: lines with id > after, oldest-first (the tail-following
/// contract used by the /logs partial and `keel logs --follow`).
pub async fn get_logs(
    State(shared): State<Arc<EngineShared>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiErr> {
    let kind = q.get("kind").map(String::as_str).unwrap_or("function");
    if kind != "function" && kind != "app" {
        return Err(bad("kind must be 'function' or 'app'"));
    }
    let refname = q.get("ref").ok_or_else(|| bad("missing param: ref"))?;
    let limit = q
        .get("limit")
        .map(|v| v.parse::<i64>().map_err(|_| bad("limit must be an integer")))
        .transpose()?
        .unwrap_or(100)
        .clamp(1, 1000);
    let after = q
        .get("after")
        .map(|v| v.parse::<i64>().map_err(|_| bad("after must be an integer")))
        .transpose()?;
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let rows = match after {
        Some(a) => db::fn_logs_after(&conn, kind, refname, a, limit),
        None => db::tail_fn_logs(&conn, kind, refname, limit),
    }
    .map_err(internal)?;
    let lines: Vec<Value> = rows
        .iter()
        .map(|l| {
            json!({
                "id": l.id,
                "invocation_id": l.invocation_id,
                "line": l.line,
                "created_at": l.created_at,
            })
        })
        .collect();
    Ok(Json(json!({"lines": lines})))
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
    // Shape lives in core (db::workflow_json) — platform-api's get-workflow
    // host call returns the identical JSON (micro-cloud Task 4.2).
    Ok(Json(db::workflow_json(&wf)))
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

// --- Micro-cloud phase 4: routes (the control plane; the /fn data plane is
// --- dispatch.rs). Token-gated like every other /api route.

/// POST /api/routes {"prefix":"/fn/echo","module_hash":"...", optional
/// fuel_limit/mem_limit/time_limit_ms} → 201. Re-POSTing a prefix re-binds
/// it (that is how the 5.6 gate lowers /fn/echo's fuel to a starvation
/// budget). Prefix rules (ext spec Task 4.3): starts with /fn/, no trailing
/// slash, no "..".
pub async fn create_route(
    State(shared): State<Arc<EngineShared>>,
    req: Request,
) -> Result<(StatusCode, Json<Value>), ApiErr> {
    // Same dual-shape story as create_schedule: JSON from scripts/curl, a
    // urlencoded form from the /routes page. Empty form limit fields = defaults.
    let body: Value = if content_type(&req).starts_with("application/x-www-form-urlencoded") {
        let Form(f) = Form::<HashMap<String, String>>::from_request(req, &())
            .await
            .map_err(bad)?;
        let mut v = serde_json::Map::new();
        for (k, val) in f {
            if val.is_empty() {
                continue;
            }
            match k.as_str() {
                "fuel_limit" | "mem_limit" | "time_limit_ms" | "rate_limit" => {
                    v.insert(
                        k,
                        Value::from(val.parse::<i64>().map_err(|_| bad("limits must be integers"))?),
                    );
                }
                _ => {
                    v.insert(k, Value::from(val));
                }
            }
        }
        Value::Object(v)
    } else {
        let Json(j) = Json::<Value>::from_request(req, &()).await.map_err(bad)?;
        j
    };
    let prefix = body
        .get("prefix")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("missing field: prefix"))?;
    let hash = body
        .get("module_hash")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("missing field: module_hash"))?;
    if !prefix.starts_with("/fn/") || prefix.len() <= 4 {
        return Err(bad("prefix must start with /fn/ and name something after it"));
    }
    if prefix.ends_with('/') {
        return Err(bad("prefix must not end with /"));
    }
    if prefix.contains("..") {
        return Err(bad("prefix must not contain .."));
    }
    let fuel = body.get("fuel_limit").and_then(Value::as_i64).unwrap_or(500_000_000);
    let mem = body.get("mem_limit").and_then(Value::as_i64).unwrap_or(64 * 1024 * 1024);
    let time_ms = body.get("time_limit_ms").and_then(Value::as_i64).unwrap_or(5000);
    if fuel <= 0 || mem <= 0 || time_ms <= 0 {
        return Err(bad("limits must be positive"));
    }
    // Amendment 1 (A1): admitted runs per rolling 60s; absent = unlimited.
    let rate = body.get("rate_limit").and_then(Value::as_i64);
    if rate.is_some_and(|r| r <= 0) {
        return Err(bad("rate_limit must be positive (omit it for unlimited)"));
    }
    // v4.0 (E4): outbound HTTP for proxy-world guests — an operator grant,
    // default deny. JSON bool; the /routes form doesn't expose it (API-only).
    let allow_outbound = body.get("allow_outbound").and_then(Value::as_bool).unwrap_or(false);
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    if !db::module_exists(&conn, hash).map_err(internal)? {
        return Err(bad(format!("unknown module hash {hash}")));
    }
    db::upsert_route(&conn, prefix, hash, fuel, mem, time_ms, rate, allow_outbound)
        .map_err(internal)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "prefix": prefix, "module_hash": hash,
            "fuel_limit": fuel, "mem_limit": mem, "time_limit_ms": time_ms,
            "rate_limit": rate, "allow_outbound": allow_outbound,
        })),
    ))
}

/// GET /api/routes → every binding with its quotas.
pub async fn list_routes(
    State(shared): State<Arc<EngineShared>>,
) -> Result<Json<Value>, ApiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let rows = db::list_routes(&conn).map_err(internal)?;
    let out: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "prefix": r.prefix, "module_hash": r.module_hash,
                "fuel_limit": r.fuel_limit, "mem_limit": r.mem_limit,
                "time_limit_ms": r.time_limit_ms, "created_at": r.created_at,
                "rate_limit": r.rate_limit, "allow_outbound": r.allow_outbound,
            })
        })
        .collect();
    Ok(Json(Value::Array(out)))
}

/// DELETE /api/routes/{*prefix} → 204 / 404. The wildcard arrives without its
/// leading slash ("fn/echo") — reattach it.
pub async fn delete_route(
    State(shared): State<Arc<EngineShared>>,
    Path(prefix): Path<String>,
) -> Result<StatusCode, ApiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    if db::delete_route(&conn, &format!("/{prefix}")).map_err(internal)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, "no such route".to_string()))
    }
}

// --- Micro-cloud phase 5: the playground judge (control plane) --------------

/// POST /api/problems — operator seeding, idempotent upsert:
/// `{"slug","title","statement","cases":[{"input","expected"},...]}`.
pub async fn upsert_problem(
    State(shared): State<Arc<EngineShared>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiErr> {
    let slug = body
        .get("slug")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("missing field: slug"))?;
    if slug.is_empty() || !slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return Err(bad("slug must be non-empty [a-z0-9-]"));
    }
    let title = body
        .get("title")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("missing field: title"))?;
    let statement = body
        .get("statement")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("missing field: statement"))?;
    let cases: Vec<(String, String)> = body
        .get("cases")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("missing field: cases (array)"))?
        .iter()
        .map(|c| {
            Some((
                c.get("input")?.as_str()?.to_string(),
                c.get("expected")?.as_str()?.to_string(),
            ))
        })
        .collect::<Option<_>>()
        .ok_or_else(|| bad("each case needs string fields input + expected"))?;
    if cases.is_empty() {
        return Err(bad("a problem needs at least one case"));
    }
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    db::upsert_problem(&conn, slug, title, statement, &cases).map_err(internal)?;
    Ok(Json(json!({"slug": slug, "cases": cases.len()})))
}

/// POST /api/submissions — two body shapes, like module upload:
///   * JSON `{"problem": slug, "module_hash": hash}` (scripts);
///   * multipart `problem` + `file` (the playground page — stores the module
///     through the same content-addressed flow first).
///
/// Returns 202 `{"id"}` immediately; judging runs on a blocking thread and
/// lands the verdict in ONE UPDATE (poll GET /api/submissions/{id}).
pub async fn create_submission(
    State(shared): State<Arc<EngineShared>>,
    req: Request,
) -> Result<(StatusCode, Json<Value>), ApiErr> {
    let (problem, hash) = if content_type(&req).starts_with("multipart/form-data") {
        let mut mp = Multipart::from_request(req, &()).await.map_err(bad)?;
        let (mut problem, mut file) = (None, None);
        while let Some(field) = mp.next_field().await.map_err(bad)? {
            match field.name() {
                Some("problem") => problem = Some(field.text().await.map_err(bad)?),
                Some("file") => file = Some(field.bytes().await.map_err(bad)?),
                _ => {}
            }
        }
        let problem = problem.ok_or_else(|| bad("missing field: problem"))?;
        let file = file.ok_or_else(|| bad("missing field: file"))?;
        if !file.starts_with(b"\0asm") {
            return Err(bad("not a WebAssembly binary (missing \\0asm magic)"));
        }
        let hash = hex::encode(Sha256::digest(&file));
        let conn = db::open_conn(&shared.db_path).map_err(internal)?;
        db::insert_module(&conn, &hash, &format!("solver-{problem}"), &file).map_err(internal)?;
        (problem, hash)
    } else {
        let Json(body) = Json::<Value>::from_request(req, &()).await.map_err(bad)?;
        (
            body.get("problem")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("missing field: problem"))?
                .to_string(),
            body.get("module_hash")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("missing field: module_hash"))?
                .to_string(),
        )
    };
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    if db::get_problem(&conn, &problem).map_err(internal)?.is_none() {
        return Err(bad(format!("unknown problem '{problem}'")));
    }
    if !db::module_exists(&conn, &hash).map_err(internal)? {
        return Err(bad(format!("unknown module hash {hash}")));
    }
    let id = uuid::Uuid::new_v4().to_string();
    db::insert_submission(&conn, &id, &problem, &hash).map_err(internal)?;
    drop(conn); // the judge thread opens its own (§E1)
    let judge_shared = shared.clone();
    let judge_id = id.clone();
    // NOT inline in the handler (ext spec Task 5.2): 202 now, verdict later.
    // v3.3 (P-FIX-2): judge runs SERIALIZE on judge_sem (1 permit) — each is
    // up to 2s × cases of blocking compute, and uncapped they park one pool
    // thread apiece. The permit is awaited in this cheap async task, so a
    // queued submission costs no thread while it waits and the 202 above is
    // unaffected.
    tokio::spawn(async move {
        let _permit = Arc::clone(&judge_shared.judge_sem)
            .acquire_owned()
            .await
            .expect("judge semaphore is never closed");
        let run_shared = judge_shared.clone();
        let run_id = judge_id.clone();
        let joined = tokio::task::spawn_blocking(move || {
            if let Err(e) = keel_core::judge::judge_submission(&run_shared, &run_id) {
                tracing::error!("judging {run_id} failed (verdict stays NULL): {e:#}");
            }
        })
        .await; // the permit is held until the blocking run finishes
        if let Err(e) = joined {
            tracing::error!("judge task for {judge_id} panicked: {e}");
        }
    });
    Ok((StatusCode::ACCEPTED, Json(json!({"id": id}))))
}

/// GET /api/submissions/{id} — verdict is null while judging; `detail` is the
/// stored per-case JSON array (a string, like workflow output).
pub async fn get_submission(
    State(shared): State<Arc<EngineShared>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let s = db::get_submission(&conn, &id)
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "unknown submission id".to_string()))?;
    Ok(Json(json!({
        "id": s.id,
        "problem": s.problem,
        "module_hash": s.module_hash,
        "verdict": s.verdict,
        "detail": s.detail,
        "created_at": s.created_at,
    })))
}

// --- Micro-cloud phase 6: hosted apps (control plane; serving is dispatch.rs)

/// POST /api/apps `{"name","backend_hash"?}` → 201. Name is `[a-z0-9-]{1,32}`;
/// a null/absent backend makes a static-only app; re-POSTing re-binds.
pub async fn create_app(
    State(shared): State<Arc<EngineShared>>,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Value>), ApiErr> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("missing field: name"))?;
    if name.is_empty()
        || name.len() > 32
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(bad("app name must be [a-z0-9-]{1,32}"));
    }
    let backend = body.get("backend_hash").and_then(Value::as_str);
    // Amendment 1 (A1): rate-limits the app's api/* backend calls only —
    // asset serving stays unmetered (it never enters a sandbox).
    let rate = body.get("rate_limit").and_then(Value::as_i64);
    if rate.is_some_and(|r| r <= 0) {
        return Err(bad("rate_limit must be positive (omit it for unlimited)"));
    }
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    if let Some(h) = backend {
        if !db::module_exists(&conn, h).map_err(internal)? {
            return Err(bad(format!("unknown module hash {h}")));
        }
    }
    let allow_outbound = body.get("allow_outbound").and_then(Value::as_bool).unwrap_or(false);
    db::upsert_app(&conn, name, backend, rate, allow_outbound).map_err(internal)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "name": name, "backend_hash": backend, "rate_limit": rate,
            "allow_outbound": allow_outbound,
        })),
    ))
}

// --- Amendment 2 (v3.5): per-ref config (A6) + kv control plane (A7) --------

/// Shared param validation for the config/kv endpoints (the /api/logs style).
fn kind_ref(q: &HashMap<String, String>) -> Result<(String, String), ApiErr> {
    let kind = q.get("kind").cloned().unwrap_or_else(|| "function".into());
    if kind != "function" && kind != "app" {
        return Err(bad("kind must be 'function' or 'app'"));
    }
    let refname = q.get("ref").cloned().ok_or_else(|| bad("missing param: ref"))?;
    Ok((kind, refname))
}

/// POST /api/config {"kind","ref","name","value"} → 201 (upsert). A6 door
/// checks live HERE — the guest surface (config-get) has no error case.
pub async fn set_config(
    State(shared): State<Arc<EngineShared>>,
    Json(body): Json<Value>,
) -> Result<StatusCode, ApiErr> {
    let get = |k: &str| -> Result<String, ApiErr> {
        body.get(k)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| bad(format!("missing field: {k}")))
    };
    let (kind, refname, name, value) = (get("kind")?, get("ref")?, get("name")?, get("value")?);
    if kind != "function" && kind != "app" {
        return Err(bad("kind must be 'function' or 'app'"));
    }
    if name.is_empty()
        || name.len() > 64
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(bad("name must be [A-Za-z0-9_-]{1,64}"));
    }
    if value.len() > 4096 {
        return Err(bad("value exceeds 4096 bytes"));
    }
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    // ≤ 64 entries per ref; overwrites of an existing name always pass.
    if db::get_config(&conn, &kind, &refname, &name).map_err(internal)?.is_none()
        && db::count_config(&conn, &kind, &refname).map_err(internal)? >= 64
    {
        return Err(bad("ref already holds 64 config entries"));
    }
    db::upsert_config(&conn, &kind, &refname, &name, &value).map_err(internal)?;
    Ok(StatusCode::CREATED)
}

/// GET /api/config?kind=&ref= → {"names":[...]} — NAMES ONLY, never values
/// (A6: the engine-side guarantee is names-out-only).
pub async fn get_config_names(
    State(shared): State<Arc<EngineShared>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiErr> {
    let (kind, refname) = kind_ref(&q)?;
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let names = db::list_config_names(&conn, &kind, &refname).map_err(internal)?;
    Ok(Json(json!({ "names": names })))
}

/// DELETE /api/config?kind=&ref=&name= → 204 / 404.
pub async fn delete_config(
    State(shared): State<Arc<EngineShared>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<StatusCode, ApiErr> {
    let (kind, refname) = kind_ref(&q)?;
    let name = q.get("name").ok_or_else(|| bad("missing param: name"))?;
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    if db::delete_config(&conn, &kind, &refname, name).map_err(internal)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, "no such config entry".to_string()))
    }
}

/// GET /api/kv?kind=&ref= → {"keys":[...]} — keys only (guest state is not
/// operator browsing material, A7).
pub async fn get_kv_keys(
    State(shared): State<Arc<EngineShared>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiErr> {
    let (kind, refname) = kind_ref(&q)?;
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let keys = db::kv_keys(&conn, &kind, &refname).map_err(internal)?;
    Ok(Json(json!({ "keys": keys })))
}

/// DELETE /api/kv?kind=&ref= → 204 — the whole ref's store ("reset my
/// function"). Idempotent: wiping an empty store is still a 204.
pub async fn wipe_kv(
    State(shared): State<Arc<EngineShared>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<StatusCode, ApiErr> {
    let (kind, refname) = kind_ref(&q)?;
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    db::kv_wipe(&conn, &kind, &refname).map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/apps — v3.4 (R.2): the OTHER API hole `keel ls` surfaced — apps
/// could be created and served but never listed except by the HTML page.
pub async fn list_apps(State(shared): State<Arc<EngineShared>>) -> Result<Json<Value>, ApiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let rows = db::list_apps(&conn).map_err(internal)?;
    let out: Vec<Value> = rows
        .iter()
        .map(|(name, backend, assets, created_at, rate_limit)| {
            json!({
                "name": name, "backend_hash": backend, "assets": assets,
                "created_at": created_at, "rate_limit": rate_limit,
            })
        })
        .collect();
    Ok(Json(Value::Array(out)))
}

/// DELETE /api/apps/{name} → 204 / 404 — v3.4 (R.2): the API hole the CLI
/// surfaced (routes have had DELETE since phase 4; apps never did). App row +
/// assets go in ONE transaction; ledger rows and captured logs remain — they
/// are history, owned by --retain-ledger-hours.
pub async fn delete_app(
    State(shared): State<Arc<EngineShared>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiErr> {
    let mut conn = db::open_conn(&shared.db_path).map_err(internal)?;
    if db::delete_app(&mut conn, &name).map_err(internal)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, format!("no app '{name}'")))
    }
}

/// POST /api/apps/{name}/assets — body = a zip of the app's dist/. Zip-slip
/// entries (`..` or absolute paths) are a 400 and nothing is stored from the
/// bundle before them; directories are skipped; `.wasm`/`.js` content types
/// are forced (browsers refuse WASM served with the wrong type).
pub async fn upload_assets(
    State(shared): State<Arc<EngineShared>>,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, ApiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    if db::get_app(&conn, &name).map_err(internal)?.is_none() {
        return Err((StatusCode::NOT_FOUND, format!("no app '{name}'")));
    }
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(body.to_vec()))
        .map_err(|e| bad(format!("not a readable zip: {e}")))?;
    // Validate EVERY entry before storing ANY — a bundle with one slip entry
    // stores nothing (all-or-nothing beats half-a-deploy). The decompressed
    // total is capped: a 64 MiB upload that inflates past 256 MiB is a zip
    // bomb, not a frontend.
    const MAX_UNPACKED: u64 = 256 * 1024 * 1024;
    let mut unpacked: u64 = 0;
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| bad(format!("zip entry {i}: {e}")))?;
        if entry.is_dir() {
            continue;
        }
        let raw = entry.name().replace('\\', "/");
        if raw.split('/').any(|seg| seg == "..") || raw.starts_with('/') {
            return Err(bad(format!(
                "zip entry '{raw}' escapes the bundle (zip-slip) — rejected"
            )));
        }
        unpacked = unpacked.saturating_add(entry.size());
        if unpacked > MAX_UNPACKED {
            return Err(bad(
                "bundle decompresses past 256 MiB — zip bomb or wrong artifact",
            ));
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes)
            .map_err(|e| bad(format!("reading zip entry '{raw}': {e}")))?;
        // entry.size() is the HEADER's claim; trust the actual bytes too.
        if bytes.len() as u64 > entry.size() && {
            unpacked = unpacked.saturating_add(bytes.len() as u64 - entry.size());
            unpacked > MAX_UNPACKED
        } {
            return Err(bad(
                "bundle decompresses past 256 MiB — zip bomb or wrong artifact",
            ));
        }
        entries.push((raw, bytes));
    }
    let stored = entries.len();
    for (path, bytes) in entries {
        // Trust but verify the two types browsers actually enforce.
        let ct = if path.ends_with(".wasm") {
            "application/wasm".to_string()
        } else if path.ends_with(".js") {
            "text/javascript".to_string()
        } else {
            mime_guess::from_path(&path)
                .first_raw()
                .unwrap_or("application/octet-stream")
                .to_string()
        };
        db::upsert_asset(&conn, &name, &path, &ct, &bytes).map_err(internal)?;
    }
    Ok(Json(json!({"stored": stored})))
}
