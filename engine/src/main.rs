// main.rs — Task 1.1: the CLI + serve() wiring for the keel binary. Since the
// v2.2 crate split, everything journal-shaped lives in keel-core (../core);
// Task 1.4's recovery scan runs inside keel_core::Engine::open.
//
// Startup order is load-bearing (SPEC.md Task 1.1): tracing → Engine::open
// (open DB + migrate + RECOVERY SCAN) → bind axum. Recovery must have issued
// its spawns before the listener accepts requests.
//
// Shutdown: workflow threads are detached; abrupt exit at ANY point (kill -9
// included) is safe because every effect commits to the journal before its result
// reaches the guest (SPEC.md §0 rule 3). Ctrl-C just stops the listener and exits.
//
// PHASE 2 (Task 2.8): ui.rs routes + /assets/* + a build.rs asserting
// assets/htmx.min.js exists.

mod api;
mod auth;
mod dispatch;
mod fleet;
mod ui;

use anyhow::Result;
use keel_core::{cron, db, host, journal, runner};
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
        /// Phase 4 (micro-cloud) — workflow fuel budget, reset to full at
        /// every run/resume. A runaway kill-switch (an infinite loop dies),
        /// not a quota: the default is minutes of continuous compute, and
        /// parked workflows spend zero. Raise it if a legitimate replay of a
        /// very long journal ever trips it (checkpoints bound replay cost).
        #[arg(long, default_value_t = 10_000_000_000_000)]
        wf_fuel_limit: u64,
        /// v1.3 — delete completed/failed workflows (and their journal, events,
        /// snapshots, kv) this many hours after they finish. 0 = keep forever.
        #[arg(long, default_value_t = 0)]
        retain_terminal_hours: u64,
        /// v2 DR — write periodic online snapshots (keel-<millis>.db) into this
        /// directory. Consistent while running; restore = copy one back over
        /// --db and start the engine.
        #[arg(long)]
        backup_dir: Option<String>,
        /// v2 DR — seconds between snapshots (with --backup-dir).
        #[arg(long, default_value_t = 300)]
        backup_interval_secs: u64,
        /// v2 DR — how many snapshots to keep in --backup-dir (oldest pruned).
        #[arg(long, default_value_t = 24)]
        backup_keep: usize,
        /// v2.1 — KEY=VALUE file backing the `secret` host call. Values never
        /// touch the database (journal records name + sha256 only; replay
        /// verifies against the live file). chmod 600 it.
        #[arg(long)]
        secrets_file: Option<String>,
        /// v2.2 — register a capability provider: name=path.wasm (repeatable).
        /// A provider is a component implementing the keel:provider world
        /// (PROVIDERS.md); guests reach it via provider-call. Compiled and
        /// type-checked at startup — a bad provider fails the boot.
        #[arg(long = "provider", value_name = "NAME=PATH")]
        providers: Vec<String>,
        /// v2.5 — register an EFFECTFUL capability provider (repeatable): may
        /// import keel:provider/host-http and make real HTTP calls, each
        /// journaled individually (PROVIDERS.md). Granting this tier means the
        /// provider can reach any URL this host can — an operator decision.
        #[arg(long = "provider-effectful", value_name = "NAME=PATH")]
        providers_effectful: Vec<String>,
    },
    /// One-shot consistent snapshot of a (possibly live) database, then exit.
    Backup {
        /// Source database path.
        #[arg(long)]
        db: String,
        /// Destination file (a standalone .db; no -wal/-shm needed).
        #[arg(long)]
        to: String,
    },
    /// Run one keel per tenant from a TOML config (v2 cell tenancy) — spawns,
    /// supervises and restarts `keel serve` children; each tenant gets its own
    /// database, port and token. Hard-killing children is safe by design (the
    /// journal), so the supervisor never asks nicely.
    Fleet {
        /// Path to the fleet config (see docs/operations.md for the format).
        #[arg(long)]
        config: String,
    },
}

/// v2.4 — tracing init, shared by serve and fleet. Default build: the plain
/// fmt subscriber, unchanged. With `--features otel` an OTLP/http span
/// exporter is layered on top — the spans themselves come from keel-core
/// (one per workflow run, one per journaled host call), so the default
/// binary pays nothing and the otel binary exports real structure. Endpoint
/// via the standard env (OTEL_EXPORTER_OTLP_ENDPOINT, default
/// http://localhost:4318). kill -9 drops unexported spans — that is the
/// engine's supported shutdown, so traces are best-effort by design.
#[cfg(not(feature = "otel"))]
fn init_tracing() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    Ok(())
}

#[cfg(feature = "otel")]
fn init_tracing() -> Result<()> {
    use opentelemetry::trace::TracerProvider as _;
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .build()?;
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name("keel")
                .build(),
        )
        .build();
    let tracer = provider.tracer("keel");
    opentelemetry::global::set_tracer_provider(provider);
    tracing_subscriber::registry()
        .with(tracing_subscriber::filter::LevelFilter::INFO)
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .init();
    Ok(())
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
            wf_fuel_limit,
            retain_terminal_hours,
            backup_dir,
            backup_interval_secs,
            backup_keep,
            secrets_file,
            providers,
            providers_effectful,
        } => {
            serve(
                db,
                listen,
                max_running,
                api_token,
                max_guest_memory_mb,
                wf_fuel_limit,
                retain_terminal_hours,
                backup_dir,
                backup_interval_secs,
                backup_keep,
                secrets_file,
                providers,
                providers_effectful,
            )
            .await
        }
        Cmd::Backup { db, to } => {
            let src = db::open_conn(&db)?;
            db::backup_to(&src, &to)?;
            println!("backed up {db} -> {to}");
            Ok(())
        }
        Cmd::Fleet { config } => fleet::run(&config).await,
    }
}

#[allow(clippy::too_many_arguments)] // 1:1 with the Serve CLI flags, nothing more
async fn serve(
    db_path: String,
    listen: String,
    max_running: u32,
    api_token: Option<String>,
    max_guest_memory_mb: usize,
    wf_fuel_limit: u64,
    retain_terminal_hours: u64,
    backup_dir: Option<String>,
    backup_interval_secs: u64,
    backup_keep: usize,
    secrets_file: Option<String>,
    providers: Vec<String>,
    providers_effectful: Vec<String>,
) -> Result<()> {
    init_tracing()?;

    if api_token.is_none() {
        tracing::warn!(
            "no --api-token / KEEL_API_TOKEN set: every endpoint is open — fine on \
             127.0.0.1, do NOT expose this listener wider without a token"
        );
    }
    // v2.1 — fail FAST on a bad secrets file: a missing/unparseable file is a
    // config error at startup, not a per-workflow surprise at 3am.
    if let Some(path) = &secrets_file {
        host::load_secrets(path).map_err(|e| anyhow::anyhow!("--secrets-file: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)?.permissions().mode();
            if mode & 0o077 != 0 {
                tracing::warn!(
                    "secrets file {path} is group/world-readable (mode {:o}) — chmod 600 it",
                    mode & 0o777
                );
            }
        }
    }
    // v2.2 — load provider bytes here (the CLI owns paths); compile/type-check
    // happens inside EngineShared::new — either way a bad provider fails boot.
    let read_provider_specs = |specs: Vec<String>, flag: &str| -> Result<Vec<(String, Vec<u8>)>> {
        let mut out = Vec::new();
        for spec in specs {
            let (name, path) = spec
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("{flag} wants name=path.wasm, got '{spec}'"))?;
            let wasm = std::fs::read(path)
                .map_err(|e| anyhow::anyhow!("{flag} {name}: reading {path}: {e}"))?;
            out.push((name.to_string(), wasm));
        }
        Ok(out)
    };
    let provider_bytes = read_provider_specs(providers, "--provider")?;
    let provider_effectful_bytes =
        read_provider_specs(providers_effectful, "--provider-effectful")?;

    // v2.2 — Engine::open = open db + migrate + RECOVERY SCAN (Task 1.4: every
    // non-terminal workflow starts again from its journal; there is no replay
    // mode). The binary and embedders now share this startup path.
    let mut opts = keel_core::EngineOptions::new(db_path.clone());
    opts.max_running = max_running;
    opts.api_token = api_token;
    opts.max_guest_memory = max_guest_memory_mb.max(1) * 1024 * 1024;
    opts.wf_fuel_limit = wf_fuel_limit;
    opts.secrets_path = secrets_file;
    opts.providers = provider_bytes;
    opts.providers_effectful = provider_effectful_bytes;
    let engine = keel_core::Engine::open(opts)?;
    let shared = engine.shared();

    // v1.2 — the scheduler loop: every second, fire due schedules by creating
    // a workflow (born 'running' BEFORE spawn — same crash-safe ordering as
    // the API path) and moving next_run_at forward. Missed windows (engine
    // downtime) collapse into ONE firing: interval schedules by whole-interval
    // math, cron schedules (v2.1) because "next" is computed from now.
    {
        let shared = shared.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let run = || -> Result<()> {
                let mut conn = db::open_conn(&shared.db_path)?;
                let now = journal::now_ms();
                for s in db::due_schedules(&conn, now)? {
                    // One decision point for both kinds (cron.rs). None means
                    // the schedule can never fire again (an impossible cron
                    // date, or an expression this engine no longer parses) —
                    // disable it rather than re-hit it every second forever.
                    let Some(next) = cron::next_run(s.cron.as_deref(), s.interval_ms, s.next_run_at, now)
                    else {
                        tracing::error!(
                            "schedule {} has no next fire time (cron: {:?}) — disabling it",
                            s.id, s.cron
                        );
                        db::set_schedule_enabled(&conn, &s.id, false)?;
                        continue;
                    };
                    let id = uuid::Uuid::new_v4().to_string();
                    // One txn: workflow row + advanced next_run_at — a crash
                    // can't double-fire a window (the row is the intent record;
                    // recovery starts it if we die before spawn).
                    db::fire_schedule(&mut conn, &s, &id, next)?;
                    tracing::info!("schedule {} fired: workflow {id}", s.id);
                    runner::spawn(shared.clone(), id); // sanctioned spawn call-site 4 of 4
                }
                Ok(())
            };
            if let Err(e) = run() {
                tracing::error!("scheduler pass failed: {e:#}");
            }
        });
    }

    // v2 DR — periodic online snapshots. First one runs immediately so a fresh
    // deployment has a restore point before anything can go wrong.
    if let Some(dir) = backup_dir {
        let db_path = db_path.clone();
        std::thread::spawn(move || loop {
            let run = || -> Result<()> {
                std::fs::create_dir_all(&dir)?;
                let src = db::open_conn(&db_path)?;
                let dest = format!("{dir}/keel-{}.db", journal::now_ms());
                db::backup_to(&src, &dest)?;
                tracing::info!("backup written: {dest}");
                // Prune: keel-<millis>.db sorts lexicographically by age
                // (fixed-width millis until the year 2286).
                let mut snaps: Vec<_> = std::fs::read_dir(&dir)?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("keel-") && n.ends_with(".db"))
                    })
                    .collect();
                snaps.sort();
                while snaps.len() > backup_keep.max(1) {
                    let old = snaps.remove(0);
                    std::fs::remove_file(&old)?;
                }
                Ok(())
            };
            if let Err(e) = run() {
                tracing::error!("backup pass failed: {e:#}");
            }
            std::thread::sleep(std::time::Duration::from_secs(backup_interval_secs.max(1)));
        });
    }

    // v1.3 — retention GC: sweep terminal workflows past the retention window.
    // First pass runs immediately (an engine restarted after long downtime
    // should not wait a minute to reclaim space), then every 60s.
    if retain_terminal_hours > 0 {
        let shared = shared.clone();
        std::thread::spawn(move || loop {
            let run = || -> Result<()> {
                let mut conn = db::open_conn(&shared.db_path)?;
                let cutoff = journal::now_ms() - (retain_terminal_hours as i64) * 3_600_000;
                let n = db::gc_terminal_workflows(&mut conn, cutoff)?;
                if n > 0 {
                    tracing::info!("retention GC removed {n} terminal workflows");
                }
                Ok(())
            };
            if let Err(e) = run() {
                tracing::error!("retention GC pass failed: {e:#}");
            }
            std::thread::sleep(std::time::Duration::from_secs(60));
        });
    }

    let app = Router::new()
        .route(
            "/api/modules",
            // Raw wasm bytes as the body; axum's ~2MB default rejects real components.
            post(api::upload_module).layer(DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        // v2.6 — the live provider registry (same body-size story as modules).
        .route(
            "/api/providers",
            post(api::upload_provider)
                .get(api::list_providers)
                .layer(DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route(
            "/api/providers/{name}",
            axum::routing::delete(api::delete_provider),
        )
        // Micro-cloud phase 4 — routes CONTROL plane (token-gated; the /fn
        // data plane mounts after the auth layer below).
        .route("/api/routes", post(api::create_route).get(api::list_routes))
        .route(
            "/api/routes/{*prefix}",
            axum::routing::delete(api::delete_route),
        )
        .route(
            "/api/workflows",
            post(api::create_workflow).get(api::list_workflows),
        )
        // v1.2 — interval schedules; v1.3 — Prometheus metrics.
        .route(
            "/api/schedules",
            post(api::create_schedule).get(api::list_schedules),
        )
        .route(
            "/api/schedules/{id}",
            axum::routing::delete(api::delete_schedule).patch(api::patch_schedule),
        )
        .route("/metrics", get(api::metrics))
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
        // v2.4 — schedules UI (create/pause/resume/delete + 2s polling).
        .route("/schedules", get(ui::schedules_page))
        .route("/providers", get(ui::providers_page))
        .route("/partials/schedules", get(ui::schedules_partial))
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
        // Micro-cloud phase 4 — the PUBLIC data plane (status.md §N.5): added
        // AFTER the auth layer, so functions (and later apps) are reachable
        // tokenless — a browser-served app must call its own backend. The
        // body cap is enforced inside the dispatcher (10 MiB → 413).
        .route("/fn/{*rest}", axum::routing::any(dispatch::dispatch_fn))
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
