// ui.rs — Task 2.8: server-rendered HTML + htmx polling partials.
//
// Templating rule (SPEC.md Task 2.8): NO askama_axum integration crate — render
// each template to a String and wrap it in axum::response::Html. That is the
// whole integration. Assets are include_bytes!-embedded and served from memory:
// the binary is self-contained offline (no CDN references anywhere).
//
// Copy rules (spec, applied everywhere): sentence case; buttons say what they do
// ("Start workflow", never "Submit"); empty states instruct; errors state cause
// and fix, no apologies.
//
// Handlers precompute display-ready strings (short ids, "3m ago", truncated
// payloads) so the templates stay logic-free.
//
// PHASE 3 (Task 3.6 step 6): the workflow detail partial grows a module select +
// "Upgrade module" button when status is sleeping/waiting_event and a snapshots
// row exists.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::Html;

use keel_core::db;
use keel_core::journal::now_ms;
use keel_core::runner::EngineShared;

type UiErr = (StatusCode, String);

fn internal(e: anyhow::Error) -> UiErr {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

// --- embedded assets ----------------------------------------------------------

pub async fn htmx_js() -> impl axum::response::IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript")],
        include_bytes!("../assets/htmx.min.js").as_slice(),
    )
}

pub async fn style_css() -> impl axum::response::IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css")],
        include_bytes!("../assets/style.css").as_slice(),
    )
}

// --- display-ready view rows ----------------------------------------------------

struct WfRow {
    id: String,
    short_id: String,
    module: String,
    status: String,
    updated: String,
}

struct ModRow {
    hash: String,
    short_hash: String,
    name: String,
    uploaded: String,
}

struct JRow {
    seq: i64,
    kind: String,
    request: String,
    response: String,
    time: String,
}

// v2.4 — schedules page + the workflow page's durable-KV section.

struct SchedRow {
    id: String,
    short_id: String,
    module: String,
    when: String, // "every 5s" | "cron */2 * * * * *"
    next: String, // "in 4s" | "due now"
    enabled: bool,
}

struct KvRow {
    key: String,
    value: String,
}

fn short(s: &str) -> String {
    s.chars().take(8).collect()
}

/// Module display label: its name, or a hash prefix for anonymous uploads.
fn module_label(name: &str, hash: &str) -> String {
    if name.is_empty() {
        short(hash)
    } else {
        name.to_string()
    }
}

/// "3s ago" / "5m ago" — a ledger needs recency, not precision.
fn ago(ts_ms: i64) -> String {
    let s = ((now_ms() - ts_ms) / 1000).max(0);
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// "in 4s" / "in 2m" / "due now" — when a schedule fires next.
fn until(ts_ms: i64) -> String {
    let s = (ts_ms - now_ms()) / 1000;
    match s {
        i64::MIN..=0 => "due now".to_string(),
        1..=59 => format!("in {s}s"),
        60..=3599 => format!("in {}m", s / 60),
        3600..=86399 => format!("in {}h", s / 3600),
        _ => format!("in {}d", s / 86400),
    }
}

/// Journal payloads can be up to 1 MiB (http-get bodies); the table shows a
/// prefix — the JSON API serves the full rows.
fn trunc(s: &str, max_chars: usize) -> String {
    if s.chars().count() > max_chars {
        format!("{}…", s.chars().take(max_chars).collect::<String>())
    } else {
        s.to_string()
    }
}

// --- templates -------------------------------------------------------------------

#[derive(Template)]
#[template(path = "dashboard.html")]
struct Dashboard {
    workflows: Vec<WfRow>,
    authed: bool,
}

#[derive(Template)]
#[template(path = "_workflows_table.html")]
struct WorkflowsTable {
    workflows: Vec<WfRow>,
}

#[derive(Template)]
#[template(path = "workflow.html")]
struct WorkflowPage {
    id: String,
    short_id: String,
    module: String,
    authed: bool,
    // fields below are consumed by the included _workflow_detail.html
    status: String,
    input: String,
    output: String,
    journal: Vec<JRow>,
    upgradable: bool,
    modules: Vec<ModRow>,
    kv: Vec<KvRow>,
}

#[derive(Template)]
#[template(path = "_workflow_detail.html")]
struct WorkflowDetail {
    id: String,
    status: String,
    input: String,
    output: String,
    journal: Vec<JRow>,
    // Task 3.6 step 6: the upgrade control renders only when parked + snapshotted.
    upgradable: bool,
    modules: Vec<ModRow>,
    kv: Vec<KvRow>,
}

#[derive(Template)]
#[template(path = "modules.html")]
struct ModulesPage {
    modules: Vec<ModRow>,
    authed: bool,
}

/// v2.6 — one provider-registry binding.
struct ProvRow {
    name: String,
    tier: &'static str,
    short_hash: String,
    hash: String,
    updated: String,
}

/// Micro-cloud phase 4 — one bound route, with its ledger count.
struct RouteUiRow {
    prefix: String,
    short_hash: String,
    hash: String,
    fuel: i64,
    mem: i64,
    time_ms: i64,
    invocations: i64,
}

#[derive(Template)]
#[template(path = "routes.html")]
struct RoutesPage {
    routes: Vec<RouteUiRow>,
    authed: bool,
}

#[derive(Template)]
#[template(path = "providers.html")]
struct ProvidersPage {
    providers: Vec<ProvRow>,
    authed: bool,
}

#[derive(Template)]
#[template(path = "schedules.html")]
struct SchedulesPage {
    schedules: Vec<SchedRow>,
    modules: Vec<ModRow>,
    authed: bool,
}

#[derive(Template)]
#[template(path = "_schedules_table.html")]
struct SchedulesTable {
    schedules: Vec<SchedRow>,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    // Empty string = no error paragraph rendered.
    error: String,
}

fn render<T: Template>(t: T) -> Result<Html<String>, UiErr> {
    Ok(Html(t.render().map_err(|e| internal(e.into()))?))
}

// --- handlers ---------------------------------------------------------------------

fn wf_rows(shared: &EngineShared) -> Result<Vec<WfRow>, UiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let rows = db::list_workflows(&conn).map_err(internal)?;
    Ok(rows
        .into_iter()
        .map(|w| WfRow {
            short_id: short(&w.id),
            id: w.id,
            module: module_label(&w.module_name, &w.module_hash),
            status: w.status,
            updated: ago(w.updated_at),
        })
        .collect())
}

/// GET /
pub async fn dashboard(State(shared): State<Arc<EngineShared>>) -> Result<Html<String>, UiErr> {
    render(Dashboard {
        workflows: wf_rows(&shared)?,
        authed: shared.api_token.is_some(),
    })
}

/// GET /partials/workflows — the dashboard's <tbody>, polled every 2s.
pub async fn workflows_partial(
    State(shared): State<Arc<EngineShared>>,
) -> Result<Html<String>, UiErr> {
    render(WorkflowsTable {
        workflows: wf_rows(&shared)?,
    })
}

fn mod_rows(conn: &keel_core::rusqlite::Connection) -> Result<Vec<ModRow>, UiErr> {
    Ok(db::list_modules(conn)
        .map_err(internal)?
        .into_iter()
        .map(|m| ModRow {
            short_hash: short(&m.hash),
            name: module_label(&m.name, &m.hash),
            hash: m.hash,
            uploaded: ago(m.created_at),
        })
        .collect())
}

fn prov_rows(conn: &keel_core::rusqlite::Connection) -> Result<Vec<ProvRow>, UiErr> {
    Ok(db::list_providers(conn)
        .map_err(internal)?
        .into_iter()
        .map(|(name, effectful, hash, updated_at)| ProvRow {
            name,
            tier: if effectful { "effectful" } else { "pure" },
            short_hash: short(&hash),
            hash,
            updated: ago(updated_at),
        })
        .collect())
}

fn sched_rows(shared: &EngineShared) -> Result<Vec<SchedRow>, UiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let mods: std::collections::HashMap<String, String> = db::list_modules(&conn)
        .map_err(internal)?
        .into_iter()
        .map(|m| (m.hash.clone(), module_label(&m.name, &m.hash)))
        .collect();
    Ok(db::list_schedules(&conn)
        .map_err(internal)?
        .into_iter()
        .map(|s| SchedRow {
            short_id: short(&s.id),
            module: mods
                .get(&s.module_hash)
                .cloned()
                .unwrap_or_else(|| short(&s.module_hash)),
            when: match &s.cron {
                Some(c) => format!("cron {c}"),
                None => format!("every {}s", s.interval_ms / 1000),
            },
            next: if s.enabled {
                until(s.next_run_at)
            } else {
                "paused".to_string()
            },
            enabled: s.enabled,
            id: s.id,
        })
        .collect())
}

/// GET /schedules
pub async fn schedules_page(
    State(shared): State<Arc<EngineShared>>,
) -> Result<Html<String>, UiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let modules = mod_rows(&conn)?;
    drop(conn);
    render(SchedulesPage {
        schedules: sched_rows(&shared)?,
        modules,
        authed: shared.api_token.is_some(),
    })
}

/// GET /partials/schedules — the schedules <tbody>, polled every 2s.
pub async fn schedules_partial(
    State(shared): State<Arc<EngineShared>>,
) -> Result<Html<String>, UiErr> {
    render(SchedulesTable {
        schedules: sched_rows(&shared)?,
    })
}

struct DetailParts {
    wf: db::WorkflowRow,
    module: String,
    journal: Vec<JRow>,
    upgradable: bool,
    modules: Vec<ModRow>,
    kv: Vec<KvRow>,
}

fn detail_parts(shared: &EngineShared, id: &str) -> Result<DetailParts, UiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let wf = db::get_workflow(&conn, id).map_err(internal)?.ok_or((
        StatusCode::NOT_FOUND,
        "unknown workflow id — check the dashboard for live ids".to_string(),
    ))?;
    let module = db::get_module_name(&conn, &wf.module_hash)
        .map_err(internal)?
        .map(|n| module_label(&n, &wf.module_hash))
        .unwrap_or_else(|| short(&wf.module_hash));
    let journal = db::journal_rows(&conn, id)
        .map_err(internal)?
        .into_iter()
        .map(|r| JRow {
            seq: r.seq,
            kind: r.kind,
            request: trunc(&r.request, 200),
            response: trunc(&r.response, 200),
            time: ago(r.created_at),
        })
        .collect();
    // Task 3.6 step 6: upgrade is offered exactly when the endpoint would accept
    // it — parked AND checkpointed.
    let upgradable = (wf.status == "sleeping" || wf.status == "waiting_event")
        && db::get_snapshot(&conn, id).map_err(internal)?.is_some();
    let modules = if upgradable { mod_rows(&conn)? } else { Vec::new() };
    // v2.4 — latest version per key (values truncated like journal payloads).
    let kv = db::kv_latest(&conn, id)
        .map_err(internal)?
        .into_iter()
        .map(|(key, value)| KvRow {
            key,
            value: trunc(&value, 200),
        })
        .collect();
    Ok(DetailParts {
        wf,
        module,
        journal,
        upgradable,
        modules,
        kv,
    })
}

fn output_display(status: &str, output: &Option<String>) -> String {
    match output {
        Some(o) => o.clone(),
        None if status == "completed" || status == "failed" => String::new(),
        None => "(not finished yet)".to_string(),
    }
}

/// GET /workflows/{id}
pub async fn workflow_page(
    State(shared): State<Arc<EngineShared>>,
    Path(id): Path<String>,
) -> Result<Html<String>, UiErr> {
    let p = detail_parts(&shared, &id)?;
    render(WorkflowPage {
        short_id: short(&p.wf.id),
        id: p.wf.id,
        module: p.module,
        authed: shared.api_token.is_some(),
        output: output_display(&p.wf.status, &p.wf.output),
        status: p.wf.status,
        input: p.wf.input,
        journal: p.journal,
        upgradable: p.upgradable,
        modules: p.modules,
        kv: p.kv,
    })
}

/// GET /partials/workflows/{id} — the detail <div>, polled every 2s.
pub async fn workflow_partial(
    State(shared): State<Arc<EngineShared>>,
    Path(id): Path<String>,
) -> Result<Html<String>, UiErr> {
    let p = detail_parts(&shared, &id)?;
    render(WorkflowDetail {
        id: p.wf.id.clone(),
        output: output_display(&p.wf.status, &p.wf.output),
        status: p.wf.status,
        input: p.wf.input,
        journal: p.journal,
        upgradable: p.upgradable,
        modules: p.modules,
        kv: p.kv,
    })
}

/// GET /modules
pub async fn modules_page(State(shared): State<Arc<EngineShared>>) -> Result<Html<String>, UiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let modules = mod_rows(&conn)?;
    render(ModulesPage {
        modules,
        authed: shared.api_token.is_some(),
    })
}

/// v2.6 — GET /providers: the live registry (upload form + bindings table).
pub async fn providers_page(
    State(shared): State<Arc<EngineShared>>,
) -> Result<Html<String>, UiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    render(ProvidersPage {
        providers: prov_rows(&conn)?,
        authed: shared.api_token.is_some(),
    })
}

/// Micro-cloud phase 4 — GET /routes: bound prefixes + quotas + ledger counts.
pub async fn routes_page(
    State(shared): State<Arc<EngineShared>>,
) -> Result<Html<String>, UiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let counts: std::collections::HashMap<String, i64> =
        db::invocation_counts(&conn, "function")
            .map_err(internal)?
            .into_iter()
            .collect();
    let routes = db::list_routes(&conn)
        .map_err(internal)?
        .into_iter()
        .map(|r| RouteUiRow {
            invocations: counts.get(&r.prefix).copied().unwrap_or(0),
            short_hash: short(&r.module_hash),
            hash: r.module_hash,
            prefix: r.prefix,
            fuel: r.fuel_limit,
            mem: r.mem_limit,
            time_ms: r.time_limit_ms,
        })
        .collect();
    render(RoutesPage {
        routes,
        authed: shared.api_token.is_some(),
    })
}

// --- v1.1 auth pages ---------------------------------------------------------------

/// GET /login — pointless without a configured token, so open mode redirects home.
pub async fn login_page(
    State(shared): State<Arc<EngineShared>>,
) -> Result<axum::response::Response, UiErr> {
    use axum::response::IntoResponse;
    if shared.api_token.is_none() {
        return Ok(axum::response::Redirect::to("/").into_response());
    }
    Ok(render(LoginPage { error: String::new() })?.into_response())
}

/// POST /login (urlencoded: token=...). Correct token → HttpOnly SameSite=Lax
/// cookie carrying the token's digest (never the raw token), redirect home.
/// Wrong token → 401 with the form again, error stated.
pub async fn login_submit(
    State(shared): State<Arc<EngineShared>>,
    axum::extract::Form(f): axum::extract::Form<std::collections::HashMap<String, String>>,
) -> Result<axum::response::Response, UiErr> {
    use axum::response::IntoResponse;
    if shared.api_token.is_none() {
        return Ok(axum::response::Redirect::to("/").into_response());
    }
    let presented = f.get("token").map(String::as_str).unwrap_or("");
    if crate::auth::login_ok(&shared, presented) {
        let cookie = format!(
            "keel_token={}; HttpOnly; SameSite=Lax; Path=/",
            crate::auth::cookie_value(shared.api_token.as_deref().unwrap_or(""))
        );
        Ok((
            [(header::SET_COOKIE, cookie)],
            axum::response::Redirect::to("/"),
        )
            .into_response())
    } else {
        let page = render(LoginPage {
            error: "wrong token — pass the value from --api-token / KEEL_API_TOKEN".to_string(),
        })?;
        Ok((StatusCode::UNAUTHORIZED, page).into_response())
    }
}

/// GET /logout — clears the cookie whether or not auth is on.
pub async fn logout() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        [(
            header::SET_COOKIE,
            "keel_token=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0".to_string(),
        )],
        axum::response::Redirect::to("/login"),
    )
        .into_response()
}
