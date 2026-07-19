// runner.rs — Task 1.3: one OS thread per active workflow + wasmtime setup.
//
// runner::spawn is called from EXACTLY four places, ever (SPEC.md §0 said
// three; v1.2 added the scheduler as the one sanctioned amendment; the v2.2
// crate split moved the first two into the lib façade without adding any):
//   1. workflow creation            (lib.rs Engine::start_workflow — the
//                                    binary's api.rs::create_workflow goes
//                                    through it too)
//   2. the startup recovery scan    (lib.rs Engine::open)
//   3. step 5 of the phase-3 upgrade handler (engine/src/api.rs)
//   4. the v1.2 scheduler loop      (engine/src/main.rs::serve)
// Adding a fifth call-site is an architecture error — don't.
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
use crate::host::{AbortForUpgrade, Ctx, Workflow, WorkflowPre};
use crate::journal::JournalCtx;
use crate::notifier::Notifier;

/// v3.3 (P-FIX-4) — the compiled-component cache, bounded. Entries carry a
/// last-touch tick; eviction removes the smallest. The cap is small (default
/// 64), so a linear eviction scan is simpler than a linked LRU and just as
/// fast. Component is Arc-backed: evicting one that live instances still use
/// is safe — they hold their own handle, the cache only drops ITS clone.
struct ComponentCache {
    map: HashMap<String, (Component, u64)>,
    tick: u64,
}

impl ComponentCache {
    /// A hit refreshes the entry's recency.
    fn get(&mut self, hash: &str) -> Option<Component> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(hash).map(|e| {
            e.1 = tick;
            e.0.clone()
        })
    }

    fn insert(&mut self, hash: &str, c: Component, cap: usize) {
        if !self.map.contains_key(hash) && self.map.len() >= cap.max(1) {
            if let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&oldest);
            }
        }
        self.tick += 1;
        let tick = self.tick;
        self.map.insert(hash.to_string(), (c, tick));
    }
}

/// Process-wide shared state (one per `keel serve`).
pub struct EngineShared {
    pub db_path: String,
    pub engine: wasmtime::Engine,
    /// Compiled-component cache keyed by module sha256. Compilation takes real
    /// time; clones out of the cache are cheap (Arc-backed). Bounded — see
    /// ComponentCache above.
    components: Mutex<ComponentCache>,
    /// v3.3 (P-FIX-4) — first-compiles serialize HERE, not under the cache
    /// lock: a cache hit never waits behind a compile, and N concurrent
    /// requests for a cold module produce ONE compile (losers re-check the
    /// cache after the winner inserts).
    compile_lock: Mutex<()>,
    /// --max-compiled-modules.
    max_components: usize,
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
    /// v1.1 — operator bearer token (--api-token / KEEL_API_TOKEN). None = open
    /// mode (loopback-intended, the v1.0 behavior). Consumed by auth.rs.
    pub api_token: Option<String>,
    /// v1.1 — per-guest linear-memory cap in bytes (--max-guest-memory-mb).
    /// Enforced via wasmtime StoreLimits; a guest that outgrows it fails.
    pub max_guest_memory: usize,
    /// v2.1 — --secrets-file path for the secret host call. None = the call
    /// errs (guest-visible) with "no secrets file configured".
    pub secrets_path: Option<String>,
    /// Phase 4 (micro-cloud) — workflow fuel budget (--wf-fuel-limit), reset
    /// to full at every run/resume; see EngineOptions.wf_fuel_limit.
    pub wf_fuel_limit: u64,
    /// v2.2 — capability providers by name; v2.5 — entries carry the
    /// operator-granted tier. v2.6 — a LIVE REGISTRY (PROVIDERS.md): backed by
    /// the providers/provider_blobs tables, mutated by the upload/rebind/
    /// delete API without a restart. RwLock because provider_call reads it per
    /// call; writers are the (rare) registry mutations.
    pub providers: Arc<std::sync::RwLock<HashMap<String, crate::provider::ProviderEntry>>>,
    /// Amendment 1 (A1) — admitted-but-not-yet-ledgered runs per (kind, ref).
    /// The ledger is the durable window state; this term only closes the gap
    /// between admission and the row landing, so a concurrent burst can't
    /// oversubscribe a limit. See function::admit.
    pub fn_inflight: Mutex<HashMap<String, u32>>,
    /// Amendment 1 (A1) — total 429'd admissions since boot (a 429 is NOT a
    /// ledger row — admission isn't a sandbox outcome). /metrics exposes it
    /// as keel_fn_rate_limited_total.
    pub fn_rate_limited: std::sync::atomic::AtomicU64,
    /// v3.3 (P-FIX-2) — the global sandbox-execution cap (--max-fn-concurrent
    /// permits). tokio's Semaphore, runtime-independent: the dispatcher
    /// try-acquires in async context so an over-cap request 503s WITHOUT ever
    /// touching the blocking pool. Arc'd for acquire_owned.
    pub fn_sem: Arc<tokio::sync::Semaphore>,
    /// v3.3 (P-FIX-2) — judge runs serialize (1 permit): each is up to
    /// 2s × cases of blocking compute, and submissions already answer 202 +
    /// poll, so queueing (awaited in a cheap async task) is honest here in a
    /// way it wouldn't be for a live HTTP caller.
    pub judge_sem: Arc<tokio::sync::Semaphore>,
    /// v3.3 (P-FIX-2) — total 503'd data-plane requests since boot; /metrics
    /// exposes it as keel_fn_over_capacity_total (no ledger row, same
    /// reasoning as fn_rate_limited).
    pub fn_over_capacity: std::sync::atomic::AtomicU64,
    /// v4.0 (E2) — detected guest world by module hash (a type walk is cheap;
    /// this makes it free). Entries are a byte each — no bound needed.
    pub guest_worlds: Mutex<HashMap<String, crate::proxy::GuestWorld>>,
    /// v4.0 (E4) — outbound HTTP requests actually made by granted refs;
    /// /metrics exposes it as keel_fn_outbound_total.
    pub fn_outbound: std::sync::atomic::AtomicU64,
}

impl EngineShared {
    pub fn new(opts: crate::EngineOptions) -> Result<Self> {
        let crate::EngineOptions {
            db_path,
            max_running,
            api_token,
            max_guest_memory,
            secrets_path,
            providers: provider_bytes,
            providers_effectful: provider_effectful_bytes,
            wf_fuel_limit,
            max_fn_concurrent,
            max_compiled_modules,
        } = opts;
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        // Post-review hardening: epoch interruption, so a guest stuck in pure
        // wasm (`loop {}`) can still be aborted. The park loops check the abort
        // flag, but a spinning guest never parks — without this, its only off
        // switch was hand-editing the database.
        config.epoch_interruption(true);
        // Phase 4 (micro-cloud, ext spec §E1): fuel on the ONE engine — the
        // per-instruction cost buys one component cache and one compilation
        // per module across workflows, providers, functions and solvers.
        // Every Store MUST set_fuel or it traps on its first instruction;
        // profiles: workflows/providers get runaway kill-switches, functions
        // and solvers get real quotas.
        config.consume_fuel(true);
        let engine = wasmtime::Engine::new(&config)?;
        {
            // Detached ticker for the process-wide Engine (clones share state).
            // 100ms per tick (ext spec §E1): function/solver deadlines are
            // ceil(time_limit_ms/100), and it bounds the cancel latency for a
            // guest that is executing wasm rather than parked in a host call.
            let engine = engine.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                engine.increment_epoch();
            });
        }
        // v2.2 — compile + type-check every provider NOW: a provider that
        // could never handle a call is a config error at startup, not a
        // guest-visible mystery later.
        // v2.6 — the registry is DB-backed (providers/provider_blobs tables).
        // Boot flags are an upload channel: validated + pre-flighted EAGERLY
        // (a bad flag provider still fails the boot, the v2.2 promise), then
        // UPSERTED — so flag-registered providers persist across restarts,
        // exactly like an API upload. Then every stored binding is loaded;
        // stored blobs that no longer compile (e.g. a wasmtime upgrade) are
        // logged and SKIPPED — the name acts unregistered (a journaled err),
        // never a bricked boot: uploads were pre-flighted, so this is rare.
        let mut providers = HashMap::new();
        {
            let conn = crate::db::open_conn(&db_path)?;
            crate::db::migrate(&conn)?; // idempotent; Engine::open migrates too
            let tiered = provider_bytes
                .into_iter()
                .map(|(n, w)| (n, w, false))
                .chain(
                    provider_effectful_bytes
                        .into_iter()
                        .map(|(n, w)| (n, w, true)),
                );
            for (name, wasm, effectful) in tiered {
                anyhow::ensure!(
                    crate::provider::valid_name(&name),
                    "provider name '{name}' must be non-empty [a-z0-9-]"
                );
                let component = crate::provider::preflight_tier(&engine, &wasm, effectful)
                    .with_context(|| format!("provider '{name}'"))?;
                anyhow::ensure!(
                    !providers.contains_key(&name),
                    "duplicate provider name '{name}'"
                );
                let hash = crate::provider::sha256_hex(&wasm);
                crate::db::upsert_provider(&conn, &name, effectful, &hash, &wasm)?;
                providers.insert(
                    name,
                    crate::provider::ProviderEntry {
                        component,
                        effectful,
                    },
                );
            }
            for (name, effectful, hash, wasm) in crate::db::load_provider_registry(&conn)? {
                if providers.contains_key(&name) {
                    continue; // registered by flag this boot — already compiled
                }
                match crate::provider::preflight_tier(&engine, &wasm, effectful) {
                    Ok(component) => {
                        providers.insert(
                            name,
                            crate::provider::ProviderEntry {
                                component,
                                effectful,
                            },
                        );
                    }
                    Err(e) => tracing::error!(
                        "stored provider '{name}' ({hash}) no longer passes pre-flight — \
                         skipping (calls will err as unregistered): {e:#}"
                    ),
                }
            }
        }
        Ok(EngineShared {
            db_path,
            engine,
            components: Mutex::new(ComponentCache {
                map: HashMap::new(),
                tick: 0,
            }),
            compile_lock: Mutex::new(()),
            max_components: max_compiled_modules.max(1),
            http: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(30))
                .build(),
            notifier: Arc::new(Notifier::new()),
            max_running: max_running.max(1), // 0 would deadlock every spawn
            running: Mutex::new(0),
            running_cv: Condvar::new(),
            threads: Mutex::new(HashMap::new()),
            upgrades: Mutex::new(std::collections::HashSet::new()),
            api_token,
            max_guest_memory,
            secrets_path,
            wf_fuel_limit,
            providers: Arc::new(std::sync::RwLock::new(providers)),
            fn_inflight: Mutex::new(HashMap::new()),
            fn_rate_limited: std::sync::atomic::AtomicU64::new(0),
            fn_sem: Arc::new(tokio::sync::Semaphore::new(max_fn_concurrent.max(1) as usize)),
            judge_sem: Arc::new(tokio::sync::Semaphore::new(1)),
            fn_over_capacity: std::sync::atomic::AtomicU64::new(0),
            guest_worlds: Mutex::new(HashMap::new()),
            fn_outbound: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// The --max-running cap (lib.rs's recovery starvation warning reads it).
    pub fn max_running(&self) -> u32 {
        self.max_running
    }

    /// v2.4 — permits currently held (threads EXECUTING or parked, not the
    /// ones still waiting on the cap). /metrics exposes it as
    /// keel_active_permits; scripts/load_test.sh asserts it never exceeds
    /// max_running.
    pub fn active_permits(&self) -> u32 {
        *self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

    /// v1.3 — /metrics: live worker threads (registry size).
    pub fn thread_count(&self) -> usize {
        self.threads.lock().unwrap().len()
    }

    /// v3.3 (P-FIX-3) — hash-first component lookup: the caller pays for the
    /// module BLOB only on a compile miss (a /fn hit used to copy MBs out of
    /// SQLite per request just to ignore them). The cache only ever holds
    /// components that compiled successfully.
    pub fn component_cached(
        &self,
        hash: &str,
        load: impl FnOnce() -> Result<Vec<u8>>,
    ) -> Result<Component> {
        if let Some(c) = self.components.lock().unwrap().get(hash) {
            return Ok(c);
        }
        // Cold path: compile OUTSIDE the cache lock (P-FIX-4) — hits above
        // never wait behind a compile. The compile lock makes a cold popular
        // module compile once; losers of the race re-check and return.
        let _compiling = self.compile_lock.lock().unwrap();
        if let Some(c) = self.components.lock().unwrap().get(hash) {
            return Ok(c);
        }
        let wasm = load()?;
        // map_err: wasmtime 43's own Error type doesn't take anyhow's .context directly
        let c = Component::new(&self.engine, &wasm)
            .map_err(anyhow::Error::from)
            .context("compiling module")?;
        self.components
            .lock()
            .unwrap()
            .insert(hash, c.clone(), self.max_components);
        Ok(c)
    }

    /// pub for the upgrade pre-flight (api.rs via preflight() below) and every
    /// caller that already holds the bytes — they only get copied on a miss.
    pub fn component(&self, hash: &str, wasm: &[u8]) -> Result<Component> {
        self.component_cached(hash, || Ok(wasm.to_vec()))
    }

    /// v3.3 — /metrics gauge keel_compiled_cache_size (the gate asserts the
    /// --max-compiled-modules bound holds from the outside).
    pub fn compiled_cache_size(&self) -> usize {
        self.components.lock().unwrap().map.len()
    }
}

/// Post-review hardening (upgrade pre-flight): prove `wasm` compiles AND matches
/// the workflow world — host-api imports, run/resume exports — WITHOUT running
/// any guest code. The upgrade endpoint calls this before its destructive
/// tail-discard txn: upgrading to a module that could never start used to brick
/// the workflow (the respawn failed, and failed is terminal). Compile is a cache
/// hit for any module that has ever run.
pub fn preflight(shared: &EngineShared, hash: &str, wasm: &[u8]) -> Result<()> {
    let component = shared.component(hash, wasm)?;
    let mut linker: Linker<Ctx> = Linker::new(&shared.engine);
    Workflow::add_to_linker::<_, HasSelf<Ctx>>(&mut linker, |c| c)?;
    let pre = linker.instantiate_pre(&component)?; // import surface check
    WorkflowPre::<Ctx>::new(pre)?; // run/resume export + type check
    Ok(())
}

/// First 8 chars — module hashes are long; spans want a label, not a key.
fn short8(s: &str) -> String {
    s.chars().take(8).collect()
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
        // Poison-tolerant: releasing a permit must never fail (a leaked permit is
        // a slot gone until restart), and the count is a plain integer — always
        // valid to touch even if some other thread panicked mid-lock.
        *self
            .0
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) -= 1;
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
        // Post-review hardening: a panic in run_workflow used to skip BOTH the
        // failed-status write and the registry self-remove, zombifying the
        // workflow (status says parked/running, no thread behind it) until the
        // next restart. Catch it, record it, always fall through to the exit
        // path. AssertUnwindSafe: shared state is all behind Mutexes, and the
        // panicked call's own state is discarded wholesale.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_workflow(&shared, &workflow_id)
        }));
        let failure = match result {
            Ok(Ok(())) => None,
            // Infrastructure failure (db unreachable, module row missing, linker
            // error...) — distinct from guest failure, which run_workflow records
            // itself. Best-effort mark failed; if even that fails we can only log.
            Ok(Err(e)) => Some(format!("runner error: {e:#}")),
            Err(p) => {
                let msg = p
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "non-string panic payload".to_string());
                Some(format!("runner panic: {msg}"))
            }
        };
        if let Some(msg) = failure {
            tracing::error!(workflow = %workflow_id, "{msg}");
            if let Ok(c) = db::open_conn(&shared.db_path) {
                let _ = db::set_status(&c, &workflow_id, "failed", Some(&msg));
            }
        }
        // Task 3.5 — the thread removes ITSELF on every exit path (completed,
        // failed, aborted, runner error, panic). Poison-tolerant: deregistering
        // a dead thread is exactly when limping past a poisoned lock is right.
        shared
            .threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&workflow_id);
        // Post-review hardening: the notifier entry would otherwise leak one Arc
        // per workflow forever. A late notify() just re-creates it — harmless.
        shared.notifier.remove_entry(&workflow_id);
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
    // v2.4 — the per-execution span every host_call span nests under.
    let _span =
        tracing::info_span!("workflow", id = %id, module = %short8(&wf.module_hash)).entered();
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
        // v1.1 — cap the guest's linear memory; growth beyond it fails, which
        // the guest sees as allocation failure (typically a trap → failed).
        limits: wasmtime::StoreLimitsBuilder::new()
            .memory_size(shared.max_guest_memory)
            .build(),
        secrets_path: shared.secrets_path.clone(),
        read_secrets: Vec::new(),
        engine: shared.engine.clone(),
        providers: shared.providers.clone(),
        max_guest_memory: shared.max_guest_memory,
    };
    let mut store = Store::new(&shared.engine, ctx);
    store.limiter(|c| &mut c.limits);
    // Phase 4 (micro-cloud) — the workflow fuel profile: reset to the FULL
    // budget at every run/resume, so replay of any segment consumes exactly
    // what the original did and never trips a limit the original survived.
    // Parked workflows spend zero (fuel is per-instruction). OutOfFuel is
    // mapped to the runaway-guest failure in the trap arm below.
    store.set_fuel(shared.wf_fuel_limit)?;
    // Post-review hardening: with epoch_interruption on, every store needs a
    // deadline. The callback re-arms it each 100ms tick unless the abort flag
    // is set — turning cancel/upgrade into a trap even for guests that never
    // park. The AbortForUpgrade in the error chain routes through the same
    // silent-exit arm as the park-loop aborts below.
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(|cx| {
        let d = cx.data();
        if d.notifier.is_aborted(&d.j.workflow_id) {
            Err(wasmtime::Error::from_anyhow(anyhow::Error::new(AbortForUpgrade)))
        } else {
            Ok(wasmtime::UpdateDeadline::Continue(1))
        }
    });
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
            // Phase 4 (micro-cloud, ext spec §E1): fuel exhaustion is the
            // runaway-guest kill-switch — its own message, because "trap:
            // all fuel consumed" reads like an engine bug, not a spin. This
            // SUPERSEDES the base spec's runaway-guest non-goal.
            if e.chain().any(|c| {
                matches!(
                    c.downcast_ref::<wasmtime::Trap>(),
                    Some(wasmtime::Trap::OutOfFuel)
                )
            }) {
                db::set_status(
                    &conn,
                    id,
                    "failed",
                    Some("runaway guest: exhausted compute budget"),
                )?;
                return Ok(());
            }
            // Other traps include journaled()'s "nondeterministic replay at seq N"
            // bail — the message survives into `output` via {:#}.
            db::set_status(&conn, id, "failed", Some(&format!("trap: {e:#}")))?
        }
    }
    Ok(())
}

/// v3.4 (R.4) — shared by runner AND function tests: a REAL EngineShared on a
/// scratch db file. Construction compiles no guests and the provider registry
/// is empty, so this stays cheap — the fact it works is what unblocked
/// admit()'s unit tests (status.md §R, the P-FIX-9 hedge).
#[cfg(test)]
pub(crate) mod testutil {
    use super::EngineShared;

    pub(crate) fn shared(name: &str, max_compiled: usize) -> EngineShared {
        let db = std::env::temp_dir().join(format!(
            "keel-test-{name}-{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&db);
        let mut opts = crate::EngineOptions::new(db.to_string_lossy());
        opts.max_compiled_modules = max_compiled;
        EngineShared::new(opts).expect("test EngineShared")
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    /// The wat text "(component)" is the smallest valid component (wasmtime's
    /// default `wat` feature accepts text bytes); cache keys are caller-
    /// supplied hashes, so one body serves every key.
    fn test_shared(name: &str, max_compiled: usize) -> super::EngineShared {
        super::testutil::shared(name, max_compiled)
    }

    fn load_counting(calls: &Cell<u32>) -> impl FnOnce() -> anyhow::Result<Vec<u8>> + '_ {
        move || {
            calls.set(calls.get() + 1);
            Ok(b"(component)".to_vec())
        }
    }

    #[test]
    fn component_cache_hit_skips_the_loader() {
        // P-FIX-3: the whole point of component_cached is that a hit never
        // pays for module bytes.
        let shared = test_shared("hit", 64);
        let calls = Cell::new(0u32);
        shared
            .component_cached("hash-a", load_counting(&calls))
            .unwrap();
        shared
            .component_cached("hash-a", load_counting(&calls))
            .unwrap();
        assert_eq!(calls.get(), 1, "second lookup must not invoke the loader");
        assert_eq!(shared.compiled_cache_size(), 1);
    }

    #[test]
    fn component_cache_evicts_least_recently_used() {
        // P-FIX-4: cap 2; touching A must make B the eviction victim when C
        // arrives, and the cache never exceeds the cap.
        let shared = test_shared("lru", 2);
        let calls = Cell::new(0u32);
        shared.component_cached("a", load_counting(&calls)).unwrap();
        shared.component_cached("b", load_counting(&calls)).unwrap();
        shared
            .component_cached("a", || panic!("a is cached — loader must not run"))
            .unwrap();
        shared.component_cached("c", load_counting(&calls)).unwrap(); // evicts b
        assert_eq!(shared.compiled_cache_size(), 2);
        shared
            .component_cached("a", || panic!("a was touched — must survive"))
            .unwrap();
        shared.component_cached("b", load_counting(&calls)).unwrap(); // miss: reload
        assert_eq!(
            calls.get(),
            4,
            "a, b, c cold + b again after eviction = 4 loads"
        );
        assert_eq!(shared.compiled_cache_size(), 2);
    }
}
