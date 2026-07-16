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
// PHASE 2 (Task 2.7): --max-running N (default 256) lands on the Serve command, with
// the >80%-permits-held starvation warning at startup. PHASE 2 (Task 2.8): ui.rs
// routes + /assets/* + a build.rs asserting assets/htmx.min.js exists.

mod api;
mod db;
mod host;
mod journal;
mod notifier;
mod runner;

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
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve { db, listen } => serve(db, listen).await,
    }
}

async fn serve(db_path: String, listen: String) -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let conn = db::open_conn(&db_path)?;
    db::migrate(&conn)?;

    let shared = Arc::new(runner::EngineShared::new(db_path.clone())?);

    // Task 1.4 — recovery scan. This IS the entire crash-recovery implementation:
    // start every non-terminal workflow from the beginning; the journal turns
    // re-execution into replay. There is no "replay mode" (SPEC.md §0 rule 2).
    for id in db::resumable_ids(&conn)? {
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
