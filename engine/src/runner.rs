// runner.rs — Task 1.3: one OS thread per active workflow + wasmtime setup.
//
// runner::spawn is called from EXACTLY three places, ever (SPEC.md §0):
//   1. workflow creation            (api.rs::create_workflow)
//   2. the startup recovery scan    (main.rs::serve)
//   3. step 5 of the phase-3 upgrade handler (not built yet)
// Adding a fourth call-site is an architecture error — don't.
//
// Status transitions (SPEC.md §5): the TERMINAL ones (→ completed | failed) live
// in the result match below and nowhere else. The parked round-trips
// (running → sleeping → running, running → waiting_event → running) live in the
// host.rs park loops (Tasks 2.4/2.5) because they flip mid-guest-call — every
// write still goes through db::set_status. Terminal: completed, failed.
//
// PHASE 3 (Task 3.5): JoinHandle registry + AbortForUpgrade sentinel check in the
// result match below (aborted threads exit silently, leaving status untouched).

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context as _, Result};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::Store;

use crate::db;
use crate::host::{AbortForUpgrade, Ctx, Workflow};
use crate::journal::JournalCtx;
use crate::notifier::Notifier;

/// Process-wide shared state (one per `keel serve`).
pub struct EngineShared {
    pub db_path: String,
    pub engine: wasmtime::Engine,
    /// Compiled-component cache keyed by module sha256. Compilation takes real time;
    /// Component is Arc-backed, so clones out of the cache are cheap.
    components: Mutex<HashMap<String, Component>>,
    /// One process-wide Agent (Arc-backed, cheap to clone); 30s per-request timeout.
    pub http: ureq::Agent,
    /// Wake-up latency optimization for parked threads + phase-3 abort flags
    /// (Task 2.3). In its own Arc so each workflow's Ctx can hold a handle.
    pub notifier: Arc<Notifier>,
    /// Task 2.7 — the --max-running permit counter (std has no semaphore; a
    /// Mutex<count> + Condvar is one). A permit is held for a workflow thread's
    /// ENTIRE life, parked included — see the starvation warning in main.rs.
    max_running: u32,
    running: Mutex<u32>,
    running_cv: Condvar,
    /// Task 3.5 — live workflow threads, so the upgrade handler (3.6) can join a
    /// parked worker after set_abort. Inserted by spawn(), removed by the thread
    /// itself on every exit path. Benign race: a thread that finishes before
    /// spawn()'s insert leaves a finished handle behind — joining it later
    /// returns instantly, and a respawn of the same id overwrites it.
    threads: Mutex<HashMap<String, std::thread::JoinHandle<()>>>,
    /// Task 3.6 — workflow ids with an upgrade in flight (step 1's claim set).
    /// api.rs's UpgradeClaim guard inserts/removes.
    pub upgrades: Mutex<std::collections::HashSet<String>>,
}

impl EngineShared {
    pub fn new(db_path: String, max_running: u32) -> Result<Self> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        Ok(EngineShared {
            db_path,
            engine: wasmtime::Engine::new(&config)?,
            components: Mutex::new(HashMap::new()),
            http: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(30))
                .build(),
            notifier: Arc::new(Notifier::new()),
            max_running: max_running.max(1), // 0 would deadlock every spawn
            running: Mutex::new(0),
            running_cv: Condvar::new(),
            threads: Mutex::new(HashMap::new()),
            upgrades: Mutex::new(std::collections::HashSet::new()),
        })
    }

    /// Task 3.6 — the upgrade handler takes a parked worker's JoinHandle to join
    /// it after set_abort. None: the thread already exited.
    pub fn take_thread(&self, id: &str) -> Option<std::thread::JoinHandle<()>> {
        self.threads.lock().unwrap().remove(id)
    }

    /// Task 3.6 — a join that timed out puts the handle BACK so a later upgrade
    /// attempt joins THIS still-running thread instead of assuming it exited
    /// (which would let step 5 race a live worker on the same journal).
    pub fn put_thread(&self, id: &str, h: std::thread::JoinHandle<()>) {
        self.threads.lock().unwrap().insert(id.to_string(), h);
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

/// Holds one --max-running permit. Released on ANY thread exit — Drop also runs
/// during a panic unwind, so a crashed workflow can't leak its permit.
struct Permit(Arc<EngineShared>);

impl Permit {
    fn acquire(shared: Arc<EngineShared>) -> Permit {
        let mut n = shared.running.lock().unwrap();
        while *n >= shared.max_running {
            n = shared.running_cv.wait(n).unwrap();
        }
        *n += 1;
        drop(n);
        Permit(shared)
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        *self.0.running.lock().unwrap() -= 1;
        self.0.running_cv.notify_all();
    }
}

/// Threads are registered (Task 3.5) but never joined in normal operation — the
/// journal (not thread lifecycle) is what makes crashes safe; kill -9 at any
/// instant is a supported shutdown (SPEC.md §0 rule 3). Only the upgrade handler
/// joins, after set_abort.
pub fn spawn(shared: Arc<EngineShared>, workflow_id: String) {
    let registry_shared = shared.clone();
    let registry_id = workflow_id.clone();
    let handle = std::thread::spawn(move || {
        // Task 2.7 — the cap blocks HERE, inside the spawned thread: creation and
        // recovery never stall on it; over-cap workflows just sit as idle threads
        // until a permit frees up.
        let _permit = Permit::acquire(shared.clone());
        if let Err(e) = run_workflow(&shared, &workflow_id) {
            // Infrastructure failure (db unreachable, module row missing, linker
            // error...) — distinct from guest failure, which run_workflow records
            // itself. Best-effort mark failed; if even that fails we can only log.
            tracing::error!(workflow = %workflow_id, "runner error: {e:#}");
            if let Ok(c) = db::open_conn(&shared.db_path) {
                let _ = db::set_status(&c, &workflow_id, "failed", Some(&format!("runner error: {e:#}")));
            }
        }
        // Task 3.5 — the thread removes ITSELF on every exit path (completed,
        // failed, aborted-for-upgrade, runner error).
        shared.threads.lock().unwrap().remove(&workflow_id);
    });
    registry_shared
        .threads
        .lock()
        .unwrap()
        .insert(registry_id, handle);
}

fn run_workflow(shared: &EngineShared, id: &str) -> Result<()> {
    // Each thread owns a private Connection (SPEC.md §1); connections are never shared.
    let conn = db::open_conn(&shared.db_path)?;
    let wf = db::get_workflow(&conn, id)?.context("workflow row missing")?;
    let wasm = db::get_module_wasm(&conn, &wf.module_hash)?.context("module blob missing")?;
    let component = shared.component(&wf.module_hash, &wasm)?;

    let mut linker: Linker<Ctx> = Linker::new(&shared.engine);
    Workflow::add_to_linker::<_, HasSelf<Ctx>>(&mut linker, |c| c)?;

    // Task 3.4 — snapshot-aware start. No snapshot: next_seq = 0 and call_run —
    // recovery is not special (§0 rule 2). Snapshot at C: hand the guest its own
    // state blob via call_resume and start the seq counter at C+1. Rows > C still
    // replay through the identical journaled() path — this is NOT a replay mode.
    let snap = db::get_snapshot(&conn, id)?;
    if let Some(s) = &snap {
        if s.module_hash != wf.module_hash {
            // A snapshot must not resume under different code: the state blob's
            // meaning is defined by the module that wrote it. The upgrade endpoint
            // (Task 3.6) is the one sanctioned way to move both together.
            db::set_status(
                &conn,
                id,
                "failed",
                Some(&format!(
                    "snapshot was written by module {} but the workflow now points at {} — \
                     refusing to resume; use POST /api/workflows/{id}/upgrade to change code",
                    s.module_hash, wf.module_hash
                )),
            )?;
            return Ok(());
        }
        // accept_phase3.sh greps engine.log for "resuming" — keep this wording.
        tracing::info!("resuming {id} from checkpoint seq {}", s.journal_seq);
    }
    let next_seq = snap.as_ref().map_or(0, |s| s.journal_seq + 1);

    let ctx = Ctx {
        j: JournalCtx {
            workflow_id: id.to_string(),
            db: conn,
            next_seq,
        },
        http: shared.http.clone(),
        notifier: shared.notifier.clone(),
        db_path: shared.db_path.clone(),
    };
    let mut store = Store::new(&shared.engine, ctx);
    let instance = Workflow::instantiate(&mut store, &component, &linker)?;

    db::set_status(&store.data().j.db, id, "running", None)?;
    let result = match &snap {
        None => instance.call_run(&mut store, &wf.input),
        Some(s) => instance.call_resume(&mut store, &s.state),
    };

    // Reclaim this thread's connection to record the outcome (store owns Ctx which
    // owns the Connection — one connection per thread, start to finish).
    let conn = store.into_data().j.db;
    match result {
        Ok(Ok(json)) => db::set_status(&conn, id, "completed", Some(&json))?,
        Ok(Err(apperr)) => db::set_status(&conn, id, "failed", Some(&apperr))?,
        Err(trap) => {
            // Task 3.5 — check the AbortForUpgrade sentinel FIRST: the upgrade
            // handler yanked this parked worker on purpose. Exit silently, status
            // untouched (still sleeping/waiting_event); the handler owns what
            // happens next. Walking the whole chain (not just root_cause) survives
            // however wasmtime::Error ↔ anyhow::Error wrapping nests the source.
            let e = anyhow::Error::from(trap);
            if e.chain()
                .any(|c| c.downcast_ref::<AbortForUpgrade>().is_some())
            {
                shared.notifier.clear_abort(id);
                return Ok(());
            }
            // Other traps include journaled()'s "nondeterministic replay at seq N"
            // bail — the message survives into `output` via {:#}.
            db::set_status(&conn, id, "failed", Some(&format!("trap: {e:#}")))?
        }
    }
    Ok(())
}
