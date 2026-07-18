// function.rs — micro-cloud phase 4: stateless serverless functions.
//
// A function is a component implementing `world handler` (../wit, 0.7.0):
// fresh sandboxed instance per request, fuel/epoch/memory quotas from the
// bound route, NO durability — no journal, no replay, direct now/random (the
// ext spec's §E1: never let journal code leak into this path). Durability is
// one door over: a function that needs reliability calls `start-workflow`,
// which starts a real journaled workflow.
//
// Bindgen #3 (after host.rs `workflow` and provider.rs's two provider worlds)
// — its own module, so the handler world's inline http-request/http-response
// records can't collide with host-api's types.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use wasmtime::component::{bindgen, HasSelf, Linker};
use wasmtime::Store;

use crate::db;
use crate::runner::EngineShared;
use crate::sandbox::{classify, MemLimiter, Outcome, Quota};

bindgen!({
    path: "../wit",
    world: "handler",
});

use keel::workflow::platform_api;

/// Store data for one function invocation. One connection per invocation
/// (db::open_conn, never shared across threads — ext spec §E1); the limiter
/// is both the cap and the meter.
pub struct FnCtx {
    pub db: rusqlite::Connection,
    pub shared: Arc<EngineShared>,
    pub mem_limiter: MemLimiter,
}

impl platform_api::Host for FnCtx {
    fn log(&mut self, msg: String) {
        tracing::info!("fn: {msg}");
    }

    fn now_ms(&mut self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn random_u64(&mut self) -> u64 {
        // Same no-new-deps trick as host.rs: a v4 uuid is 122 random bits;
        // unlike the workflow version there's no journal/textual-compare
        // constraint here, so the full 64 bits pass through.
        u64::from_le_bytes(uuid::Uuid::new_v4().as_bytes()[..8].try_into().unwrap())
    }

    fn start_workflow(&mut self, module_hash: String, input: String) -> Result<String, String> {
        let known = db::module_exists(&self.db, &module_hash)
            .map_err(|e| format!("engine error: {e:#}"))?;
        if !known {
            return Err(format!("unknown module hash {module_hash}"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        // Same crash-safe order as Engine::start_workflow: the row COMMITS
        // before the spawn (die between the two → recovery picks it up).
        db::create_workflow(&self.db, &id, &module_hash, &input)
            .map_err(|e| format!("engine error: {e:#}"))?;
        // sanctioned spawn call-site 5 of 5 (§0, amended by ext spec Task 4.2)
        crate::runner::spawn(self.shared.clone(), id.clone());
        Ok(id)
    }

    fn get_workflow(&mut self, id: String) -> Result<String, String> {
        match db::get_workflow(&self.db, &id) {
            Ok(Some(wf)) => Ok(db::workflow_json(&wf).to_string()),
            Ok(None) => Err(format!("unknown workflow id {id}")),
            Err(e) => Err(format!("engine error: {e:#}")),
        }
    }
}

/// Everything the ledger + HTTP layer need to know about one invocation.
/// `response` is Some only for Outcome::Ok.
pub struct Invocation {
    pub outcome: Outcome,
    pub response: Option<HttpResponse>,
    pub fuel_used: u64,
    pub peak_mem: usize,
    pub duration_ms: u64,
}

/// The dispatcher core (ext spec Tasks 4.3/6.1 share it): run ONE request
/// through a handler component under the given quotas, classify the outcome,
/// and ALWAYS write the `invocations` ledger row — metering that only counts
/// successes is fiction, so the row is written on every path, failure
/// included. `kind` is 'function'|'app'; `refname` is the route prefix or app
/// name. Err = the ENGINE failed (db/compile trouble), not the guest.
pub fn invoke_handler(
    shared: &Arc<EngineShared>,
    kind: &str,
    refname: &str,
    module_hash: &str,
    quota: Quota,
    req: HttpRequest,
) -> Result<Invocation> {
    let conn = db::open_conn(&shared.db_path)?;
    let wasm = db::get_module_wasm(&conn, module_hash)?
        .with_context(|| format!("module {module_hash} not found"))?;
    let component = shared.component(module_hash, &wasm)?;

    let ctx = FnCtx {
        db: conn,
        shared: shared.clone(),
        mem_limiter: MemLimiter {
            limit: quota.mem,
            peak: 0,
            denied: false,
        },
    };
    let mut store = Store::new(&shared.engine, ctx);
    store.limiter(|c| &mut c.mem_limiter);
    // The FUNCTION limit profile (ext spec §E1): real quotas on both
    // dimensions — fuel for compute, epoch deadline for wall time (deadline
    // expiry TRAPS: no callback, unlike workflow/provider stores).
    store.set_fuel(quota.fuel)?;
    store.set_epoch_deadline(quota.time_ms.div_ceil(100).max(1));
    store.epoch_deadline_trap();

    let started = std::time::Instant::now();
    let result = (|| -> Result<HttpResponse, wasmtime::Error> {
        let mut linker: Linker<FnCtx> = Linker::new(&shared.engine);
        Handler::add_to_linker::<_, HasSelf<FnCtx>>(&mut linker, |c| c)?;
        let h = Handler::instantiate(&mut store, &component, &linker)?;
        h.call_handle(&mut store, &req)
    })();
    let duration_ms = started.elapsed().as_millis() as u64;

    let fuel_used = quota.fuel - store.get_fuel().unwrap_or(0);
    let ctx = store.into_data();
    let outcome = classify(&result, &ctx.mem_limiter);
    db::insert_invocation(
        &ctx.db,
        kind,
        refname,
        module_hash,
        outcome.as_str(),
        Some(fuel_used as i64),
        Some(ctx.mem_limiter.peak as i64),
        duration_ms as i64,
    )
    .context("recording invocation")?;
    Ok(Invocation {
        outcome,
        response: result.ok(),
        fuel_used,
        peak_mem: ctx.mem_limiter.peak,
        duration_ms,
    })
}
