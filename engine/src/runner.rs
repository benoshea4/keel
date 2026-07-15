// runner.rs — Task 1.3: one OS thread per active workflow + wasmtime setup.
//
// runner::spawn is called from EXACTLY three places, ever (SPEC.md §0):
//   1. workflow creation            (api.rs::create_workflow)
//   2. the startup recovery scan    (main.rs::serve)
//   3. step 5 of the phase-3 upgrade handler (not built yet)
// Adding a fourth call-site is an architecture error — don't.
//
// Status transitions live HERE and nowhere else (SPEC.md §5):
//   running → completed | failed | sleeping* | waiting_event*      (* = phase 2)
//   sleeping → running;  waiting_event → running.
//   Terminal: completed, failed.
//
// PHASE 2 (Task 2.7): the --max-running permit counter wraps the thread body.
// PHASE 3 (Task 3.4): snapshot-aware start — next_seq = snapshot.journal_seq + 1 and
// call_resume(state) instead of call_run(input) when a snapshots row exists.
// PHASE 3 (Task 3.5): JoinHandle registry + AbortForUpgrade sentinel check in the
// result match below (aborted threads exit silently, leaving status untouched).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::Store;

use crate::db;
use crate::host::{Ctx, Workflow};
use crate::journal::JournalCtx;

/// Process-wide shared state (one per `keel serve`).
pub struct EngineShared {
    pub db_path: String,
    pub engine: wasmtime::Engine,
    /// Compiled-component cache keyed by module sha256. Compilation takes real time;
    /// Component is Arc-backed, so clones out of the cache are cheap.
    components: Mutex<HashMap<String, Component>>,
    /// One process-wide Agent (Arc-backed, cheap to clone); 30s per-request timeout.
    pub http: ureq::Agent,
    // PHASE 2 adds: notifier: Notifier, max_running permit counter.
}

impl EngineShared {
    pub fn new(db_path: String) -> Result<Self> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        Ok(EngineShared {
            db_path,
            engine: wasmtime::Engine::new(&config)?,
            components: Mutex::new(HashMap::new()),
            http: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(30))
                .build(),
        })
    }

    fn component(&self, hash: &str, wasm: &[u8]) -> Result<Component> {
        let mut cache = self.components.lock().unwrap();
        if let Some(c) = cache.get(hash) {
            return Ok(c.clone());
        }
        // Compiling while holding the lock serializes first-compiles of distinct
        // modules — accepted at hobby scale (SPEC.md §1: simple and correct first).
        // map_err: wasmtime 43's own Error type doesn't take anyhow's .context directly
        let c = Component::new(&self.engine, wasm)
            .map_err(anyhow::Error::from)
            .context("compiling module")?;
        cache.insert(hash.to_string(), c.clone());
        Ok(c)
    }
}

/// Threads are DETACHED on purpose: the journal (not thread lifecycle) is what makes
/// crashes safe — kill -9 at any instant is a supported shutdown (SPEC.md §0 rule 3).
pub fn spawn(shared: Arc<EngineShared>, workflow_id: String) {
    std::thread::spawn(move || {
        if let Err(e) = run_workflow(&shared, &workflow_id) {
            // Infrastructure failure (db unreachable, module row missing, linker
            // error...) — distinct from guest failure, which run_workflow records
            // itself. Best-effort mark failed; if even that fails we can only log.
            tracing::error!(workflow = %workflow_id, "runner error: {e:#}");
            if let Ok(c) = db::open_conn(&shared.db_path) {
                let _ = db::set_status(&c, &workflow_id, "failed", Some(&format!("runner error: {e:#}")));
            }
        }
    });
}

fn run_workflow(shared: &EngineShared, id: &str) -> Result<()> {
    // Each thread owns a private Connection (SPEC.md §1); connections are never shared.
    let conn = db::open_conn(&shared.db_path)?;
    let wf = db::get_workflow(&conn, id)?.context("workflow row missing")?;
    let wasm = db::get_module_wasm(&conn, &wf.module_hash)?.context("module blob missing")?;
    let component = shared.component(&wf.module_hash, &wasm)?;

    let mut linker: Linker<Ctx> = Linker::new(&shared.engine);
    Workflow::add_to_linker::<_, HasSelf<Ctx>>(&mut linker, |c| c)?;

    // next_seq is ALWAYS 0 — recovery is not special; replay happens row-by-row
    // inside journaled() (SPEC.md §0 rule 2). Phase 3's snapshot resume is the only
    // exception (next_seq = snapshot.journal_seq + 1, call_resume instead of run).
    let ctx = Ctx {
        j: JournalCtx {
            workflow_id: id.to_string(),
            db: conn,
            next_seq: 0,
        },
        http: shared.http.clone(),
    };
    let mut store = Store::new(&shared.engine, ctx);
    let instance = Workflow::instantiate(&mut store, &component, &linker)?;

    db::set_status(&store.data().j.db, id, "running", None)?;
    let result = instance.call_run(&mut store, &wf.input);

    // Reclaim this thread's connection to record the outcome (store owns Ctx which
    // owns the Connection — one connection per thread, start to finish).
    let conn = store.into_data().j.db;
    match result {
        Ok(Ok(json)) => db::set_status(&conn, id, "completed", Some(&json))?,
        Ok(Err(apperr)) => db::set_status(&conn, id, "failed", Some(&apperr))?,
        // Traps include journaled()'s "nondeterministic replay at seq N" bail — the
        // message survives into `output` via {:#}. PHASE 3 (Task 3.5): check for the
        // AbortForUpgrade sentinel FIRST here, before marking anything failed.
        Err(trap) => db::set_status(&conn, id, "failed", Some(&format!("trap: {trap:#}")))?,
    }
    Ok(())
}
