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

use crate::db;
use crate::journal::now_ms;
use crate::runner::EngineShared;

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
    // fields below are consumed by the included _workflow_detail.html
    status: String,
    input: String,
    output: String,
    journal: Vec<JRow>,
    upgradable: bool,
    modules: Vec<ModRow>,
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
}

#[derive(Template)]
#[template(path = "modules.html")]
struct ModulesPage {
    modules: Vec<ModRow>,
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

fn mod_rows(conn: &rusqlite::Connection) -> Result<Vec<ModRow>, UiErr> {
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

struct DetailParts {
    wf: db::WorkflowRow,
    module: String,
    journal: Vec<JRow>,
    upgradable: bool,
    modules: Vec<ModRow>,
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
    Ok(DetailParts {
        wf,
        module,
        journal,
        upgradable,
        modules,
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
        output: output_display(&p.wf.status, &p.wf.output),
        status: p.wf.status,
        input: p.wf.input,
        journal: p.journal,
        upgradable: p.upgradable,
        modules: p.modules,
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
    })
}

/// GET /modules
pub async fn modules_page(State(shared): State<Arc<EngineShared>>) -> Result<Html<String>, UiErr> {
    let conn = db::open_conn(&shared.db_path).map_err(internal)?;
    let modules = mod_rows(&conn)?;
    render(ModulesPage { modules })
}
