// judge.rs — micro-cloud phase 5: the playground judge.
//
// A submission is a component implementing `world solver` — NO imports at
// all, the tightest sandbox in the platform: the module cannot name a single
// external capability, so the linker is empty and the only things it can
// spend are the quotas below. Verdicts are the classic judge alphabet:
//   AC (output == expected, trimmed)   WA (ran fine, wrong answer)
//   TLE (epoch deadline)  MLE (memory limiter)  OOF (fuel)  RE (trap or
//   guest-returned Err)
//
// Bindgen #4. Cases run in stored idx order; judging STOPS at the first
// non-AC (ext spec Task 5.2); every case writes a `solve` ledger row —
// untrusted code is exactly what the meter exists for.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use wasmtime::component::{bindgen, Linker};
use wasmtime::Store;

use crate::db;
use crate::runner::EngineShared;
use crate::sandbox::{classify, MemLimiter, Outcome};

bindgen!({
    path: "../wit",
    world: "solver",
});

/// Per-case quotas (ext spec Task 5.2). Constants, not knobs: the playground
/// is one operator's judge, and identical budgets are what make verdicts
/// comparable across submissions.
pub const CASE_FUEL: u64 = 1_000_000_000;
pub const CASE_MEM: usize = 256 * 1024 * 1024;
pub const CASE_TIME_MS: u64 = 2000;

/// Solver stores carry nothing but the meter — there is no host surface.
struct SolverCtx {
    mem_limiter: MemLimiter,
}

/// Judge one submission to its final verdict. Runs on a blocking thread (the
/// API returns 202 first); the single UPDATE at the end is what flips the
/// UI's "judging…" row. Err = engine trouble (db/compile), which leaves the
/// verdict NULL — visible, and re-submittable.
pub fn judge_submission(shared: &Arc<EngineShared>, submission_id: &str) -> Result<()> {
    let conn = db::open_conn(&shared.db_path)?;
    let sub = db::get_submission(&conn, submission_id)?
        .with_context(|| format!("submission {submission_id} vanished"))?;
    let cases = db::list_cases(&conn, &sub.problem)?;
    anyhow::ensure!(!cases.is_empty(), "problem {} has no cases", sub.problem);
    let wasm = db::get_module_wasm(&conn, &sub.module_hash)?
        .with_context(|| format!("module {} not found", sub.module_hash))?;
    let component = shared.component(&sub.module_hash, &wasm)?;

    let mut detail: Vec<serde_json::Value> = Vec::new();
    let mut final_verdict = "AC";
    for (idx, input, expected) in &cases {
        let ctx = SolverCtx {
            mem_limiter: MemLimiter {
                limit: CASE_MEM,
                peak: 0,
                denied: false,
            },
        };
        let mut store = Store::new(&shared.engine, ctx);
        store.limiter(|c| &mut c.mem_limiter);
        store.set_fuel(CASE_FUEL)?;
        store.set_epoch_deadline(CASE_TIME_MS.div_ceil(100).max(1));
        store.epoch_deadline_trap();

        let started = std::time::Instant::now();
        let result = (|| -> Result<Result<String, String>, wasmtime::Error> {
            // World solver imports NOTHING — the empty linker IS the sandbox.
            let linker: Linker<SolverCtx> = Linker::new(&shared.engine);
            let s = Solver::instantiate(&mut store, &component, &linker)?;
            s.call_solve(&mut store, input)
        })();
        let ms = started.elapsed().as_millis() as i64;
        let fuel_used = (CASE_FUEL - store.get_fuel().unwrap_or(0)) as i64;
        let ctx = store.into_data();

        // classify() sees the sandbox outcome; the guest's own Err (a solve
        // that RETURNED failure) is RE with ledger outcome guest_error.
        let outcome = classify(&result, &ctx.mem_limiter);
        let (verdict, ledger) = match (outcome, &result) {
            (Outcome::Mle, _) => ("MLE", Outcome::Mle),
            (Outcome::Oof, _) => ("OOF", Outcome::Oof),
            (Outcome::Tle, _) => ("TLE", Outcome::Tle),
            (Outcome::Trap, _) => ("RE", Outcome::Trap),
            (Outcome::Ok, Ok(Ok(out))) => {
                if out.trim() == expected.trim() {
                    ("AC", Outcome::Ok)
                } else {
                    ("WA", Outcome::Ok)
                }
            }
            (Outcome::Ok, Ok(Err(_))) => ("RE", Outcome::GuestError),
            // classify returned Ok, so result cannot be Err — but write it
            // down rather than panic a judge thread if that ever changes.
            (Outcome::Ok, Err(_)) => ("RE", Outcome::Trap),
            (Outcome::GuestError, _) => unreachable!("classify never returns GuestError"),
        };
        db::insert_invocation(
            &conn,
            "solve",
            submission_id,
            &sub.module_hash,
            ledger.as_str(),
            Some(fuel_used),
            Some(ctx.mem_limiter.peak as i64),
            ms,
        )?;
        detail.push(serde_json::json!({
            "idx": idx,
            "verdict": verdict,
            "fuel": fuel_used,
            "peak_mem": ctx.mem_limiter.peak,
            "ms": ms,
        }));
        if verdict != "AC" {
            final_verdict = verdict;
            break;
        }
    }
    db::set_submission_verdict(
        &conn,
        submission_id,
        final_verdict,
        &serde_json::Value::Array(detail).to_string(),
    )?;
    tracing::info!("judged submission {submission_id}: {final_verdict}");
    Ok(())
}
