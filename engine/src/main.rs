// main.rs — Task 1.1 (CLI, startup order) + Task 1.4 (recovery scan).
//
// Startup order is load-bearing (SPEC.md Task 1.1): tracing → open DB + migrate →
// RECOVERY SCAN → bind axum. Recovery must have issued its spawns before the
// listener accepts requests.
//
// Shutdown: workflow threads are detached; abrupt exit at ANY point (kill -9
// included) is safe because every effect commits to the journal before its result
// reaches the guest (SPEC.md §0 rule 3). Ctrl-C just stops the listener and exits.
//
// PHASE 2 (Task 2.8): ui.rs routes + /assets/* + a build.rs asserting
// assets/htmx.min.js exists.

mod api;
mod auth;
mod db;
mod host;
mod journal;
mod notifier;
mod runner;
mod ui;

use std::sync::Arc;

use anyhow::Result;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "keel", about = "Keel — a durable WASM workflow engine")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the engine (JSON API + workflow threads).
    Serve {
        /// Path to the SQLite database (created if missing).
        #[arg(long, default_value = "keel.db")]
        db: String,
        /// Listen address.
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: String,
        /// Maximum concurrently active workflow threads (Task 2.7). Parked
        /// (sleeping/waiting-event) workflows still hold their permit — keep this
        /// generously above your total live workflow count.
        #[arg(long, default_value_t = 256)]
        max_running: u32,
        /// v1.1 — operator bearer token. When set, every API call needs
        /// `Authorization: Bearer <token>` and the UI needs a login. Prefer the
        /// env var so the token stays out of `ps` output. Unset = open mode
        /// (loopback only — see "Scope and security posture" in the README).
        #[arg(long, env = "KEEL_API_TOKEN")]
        api_token: Option<String>,
        /// v1.1 — per-guest linear-memory cap in MiB. A guest that outgrows it
        /// fails (allocation error → trap) instead of eating the host.
        #[arg(long, default_value_t = 256)]
        max_guest_memory_mb: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve {
            db,
            listen,
            max_running,
            api_token,
            max_guest_memory_mb,
        } => serve(db, listen, max_running, api_token, max_guest_memory_mb).await,
    }
}

async fn serve(
    db_path: String,
    listen: String,
    max_running: u32,
    api_token: Option<String>,
    max_guest_memory_mb: usize,
) -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let conn = db::open_conn(&db_path)?;
    db::migrate(&conn)?;

    if api_token.is_none() {
        tracing::warn!(
            "no --api-token / KEEL_API_TOKEN set: every endpoint is open — fine on \
             127.0.0.1, do NOT expose this listener wider without a token"
        );
    }
    let shared = Arc::new(runner::EngineShared::new(
        db_path.clone(),
        max_running,
        api_token,
        max_guest_memory_mb.max(1) * 1024 * 1024,
    )?);

    // Task 1.4 — recovery scan. This IS the entire crash-recovery implementation:
    // start every non-terminal workflow from the beginning; the journal turns
    // re-execution into replay. There is no "replay mode" (SPEC.md §0 rule 2).
    let resumable = db::resumable_ids(&conn)?;
    // Task 2.7 starvation warning: parked workflows hold permits in this design,
    // so a recovery that nearly fills the cap starves workflow N+1 of a thread
    // until something finishes. (>80%, in integer math: n/max > 4/5.)
    if resumable.len() as u64 * 5 > max_running.max(1) as u64 * 4 {
        tracing::warn!(
            "recovering {} workflows against --max-running {}: parked (sleeping/waiting) \
             workflows still hold permits, so new or recovered workflows beyond the cap \
             will starve. Parked OS threads are cheap — raise --max-running well above \
             your workflow count.",
            resumable.len(),
            max_running
        );
    }
    for id in resumable {
        tracing::info!("recovering workflow {id}");
        runner::spawn(shared.clone(), id); // sanctioned spawn call-site 2 of 3 (§0)
    }
    drop(conn); // startup connection is done; every thread opens its own

    let app = Router::new()
        .route(
            "/api/modules",
            // Raw wasm bytes as the body; axum's ~2MB default rejects real components.
            post(api::upload_module).layer(DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route("/api/workflows", post(api::create_workflow))
        // NOTE: axum 0.8 path-param syntax is {id}. The spec's route table shows the
        // 0.7-era ":id", which PANICS at router build time in 0.8 (status.md dev. 1).
        .route("/api/workflows/{id}", get(api::get_workflow))
        .route("/api/workflows/{id}/journal", get(api::get_journal))
        .route("/api/workflows/{id}/events", post(api::post_event))
        .route("/api/workflows/{id}/upgrade", post(api::upgrade_workflow))
        .route("/api/workflows/{id}/cancel", post(api::cancel_workflow))
        // Task 2.8 — server-rendered UI + polling partials + embedded assets.
        .route("/", get(ui::dashboard))
        .route("/partials/workflows", get(ui::workflows_partial))
        .route("/workflows/{id}", get(ui::workflow_page))
        .route("/partials/workflows/{id}", get(ui::workflow_partial))
        .route("/modules", get(ui::modules_page))
        .route("/assets/htmx.min.js", get(ui::htmx_js))
        .route("/assets/style.css", get(ui::style_css))
        // v1.1 — auth. The middleware wraps every route above; /login, /logout
        // and /assets/* are allowlisted inside it. No token configured → no-op.
        .route("/login", get(ui::login_page).post(ui::login_submit))
        .route("/logout", get(ui::logout))
        .layer(axum::middleware::from_fn_with_state(
            shared.clone(),
            auth::require_auth,
        ))
        .with_state(shared);

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!("keel listening on http://{listen} (db: {db_path})");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("ctrl-c: exiting (detached workflow threads die with the process; the journal makes that safe)");
        })
        .await?;
    Ok(())
}
