// host.rs — Task 1.2: wasmtime component bindings + the host-api implementation.
//
// Every effectful host function below goes through JournalCtx::journaled — that
// wrapper is what makes calls durable and replayable. `log` is the deliberate
// exception: it claims NO seq and writes NO journal row (SPEC.md §4.1); replay
// re-runs it, so duplicate log lines after a restart are expected and harmless.
//
// Journal payload JSON is fixed by SPEC.md §4.2 — the field names ("ok"/"err"/"ms"/
// "v") are part of the on-disk format; renaming them breaks replay of existing DBs.
//
// PHASE 2: Task 2.5 fills in the await_event body (park loop + single-txn event
// delivery); Task 2.6 adds retries inside do_http_get (live path only).
// PHASE 3: Task 3.3 adds checkpoint.

use std::io::Read;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use wasmtime::component::bindgen;

use crate::db;
use crate::journal::{now_ms, JournalCtx};
use crate::notifier::Notifier;

bindgen!({
    path: "../wit",           // keel/wit/workflow.wit, relative to engine/
    world: "workflow",
    // Host fns return wasmtime::Result<_> so a journal/db error traps the guest
    // cleanly (workflow -> failed). NOTE: the spec-era option `trappable_imports:
    // true` was renamed in wasmtime 43 to this syntax (see status.md deviation 2).
    imports: { default: trappable },
});
// FALLBACK (SPEC.md Task 1.2): if bindgen! can't find ../wit, copy wit/ into
// engine/wit/ and use path: "wit" — keep the two copies in sync.

/// Per-workflow store data — everything a host call can touch.
pub struct Ctx {
    pub j: JournalCtx,
    pub http: ureq::Agent,
    /// Park-loop wake-ups + the phase-3 abort flag (Task 2.3).
    pub notifier: Arc<Notifier>,
}

/// Sentinel error the park loops (sleep_ms, await_event) bail with when the
/// notifier's abort flag is set. Nothing sets that flag until PHASE 3: Task 3.6's
/// upgrade endpoint sets it, and Task 3.5's runner result-match downcasts to this
/// type (via anyhow root_cause/chain) to exit the thread WITHOUT marking the
/// workflow failed. Defined as a real error type now so those downcasts work
/// without reworking the loops later.
#[derive(Debug)]
pub struct AbortForUpgrade;

impl std::fmt::Display for AbortForUpgrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AbortForUpgrade")
    }
}

impl std::error::Error for AbortForUpgrade {}

/// Serializes to `{}` — the §4.2 request/response shape for parameterless calls.
#[derive(Serialize, Deserialize)]
struct Empty {}

/// wasmtime 43 split its Error type from anyhow (the spec predates this): the
/// engine stays on anyhow internally (journal.rs etc.), and this is the one
/// conversion point where a journal/db failure becomes a guest trap.
/// wasmtime::Error -> anyhow::Error is automatic (`?`); this direction is not.
fn trap(e: anyhow::Error) -> wasmtime::Error {
    wasmtime::Error::from_anyhow(e)
}

impl keel::workflow::host_api::Host for Ctx {
    fn http_get(&mut self, url: String) -> wasmtime::Result<Result<String, String>> {
        #[derive(Serialize)]
        struct Req {
            url: String,
        }
        // Guest-visible errors (bad status, transport failure) are DATA, not traps:
        // they journal as {"err": ...} and replay identically. Untagged enum gives
        // the exact §4.2 JSON: {"ok": body} | {"err": message}.
        #[derive(Serialize, Deserialize)]
        #[serde(untagged)]
        enum Resp {
            Ok { ok: String },
            Err { err: String },
        }

        let agent = self.http.clone(); // Agent is Arc-backed: cheap clone keeps the
        let u = url.clone();           // closure from borrowing self while j is &mut
        let r = self.j.journaled("http-get", &Req { url }, move || {
            Ok(match do_http_get(&agent, &u) {
                Ok(body) => Resp::Ok { ok: body },
                Err(e) => Resp::Err { err: e },
            })
        }).map_err(trap)?;
        Ok(match r {
            Resp::Ok { ok } => Ok(ok),
            Resp::Err { err } => Err(err),
        })
    }

    fn sleep_ms(&mut self, ms: u64) -> wasmtime::Result<()> {
        // Task 2.4 — durable sleep. Hand-rolled instead of journaled() because a
        // park loop sits between the replay check and the journal commit; the §0
        // invariants are unchanged (replay check first, row commits before return).
        //
        // Durability: the FIRST arrival at this seq writes a timers row with an
        // ABSOLUTE wake_at, then parks. kill -9 mid-sleep → recovery replays to
        // this same seq, finds no journal row but an existing timers row, KEEPS its
        // wake_at, and parks only for the remainder (the phase-1 full-re-sleep wart
        // is gone). The journal row commits only on wake; a crash between the
        // timers DELETE and that INSERT re-runs a full sleep — the documented §6
        // at-least-once caveat.
        #[derive(Serialize)]
        struct Req {
            ms: u64,
        }
        let seq = self.j.next_seq;
        self.j.next_seq += 1;
        let id = self.j.workflow_id.clone();
        let req_json = serde_json::to_string(&Req { ms }).map_err(|e| trap(e.into()))?;

        // Replay path — the same verification journaled() performs.
        if let Some((rkind, rreq, _)) = db::get_journal_row(&self.j.db, &id, seq).map_err(trap)? {
            if rkind != "sleep-ms" || rreq != req_json {
                return Err(trap(anyhow::anyhow!(
                    "nondeterministic replay at seq {seq}: recorded ({rkind}, {rreq}) \
                     but live code produced (sleep-ms, {req_json}). The workflow code \
                     has diverged from its journal."
                )));
            }
            return Ok(()); // recorded response is {} — nothing to surface
        }

        // Live path: get-or-create the durable deadline, then park until it passes.
        let wake_at = match db::get_timer_wake_at(&self.j.db, &id).map_err(trap)? {
            Some(w) => w, // restart mid-sleep: keep the original deadline
            None => {
                let w = now_ms() + ms as i64;
                db::insert_timer(&self.j.db, &id, seq, w).map_err(trap)?;
                w
            }
        };
        db::set_status(&self.j.db, &id, "sleeping", None).map_err(trap)?;
        loop {
            if self.notifier.is_aborted(&id) {
                return Err(trap(anyhow::Error::new(AbortForUpgrade)));
            }
            let remaining = wake_at - now_ms();
            if remaining <= 0 {
                break;
            }
            // 1s cap keeps the DB (via now_ms drift) re-checked even if every
            // notify is lost — the Notifier is a latency optimization only.
            self.notifier
                .wait(&id, std::time::Duration::from_millis(remaining.min(1000) as u64));
        }
        // Final re-check: never commit completion after an abort (SPEC.md Task 2.4).
        if self.notifier.is_aborted(&id) {
            return Err(trap(anyhow::Error::new(AbortForUpgrade)));
        }
        db::delete_timer(&self.j.db, &id).map_err(trap)?;
        db::insert_journal_row(&self.j.db, &id, seq, "sleep-ms", &req_json, "{}").map_err(trap)?;
        db::set_status(&self.j.db, &id, "running", None).map_err(trap)?;
        Ok(())
    }

    fn now_ms(&mut self) -> wasmtime::Result<u64> {
        #[derive(Serialize, Deserialize)]
        struct Resp {
            ms: i64,
        }
        let r = self.j.journaled("now-ms", &Empty {}, || Ok(Resp { ms: now_ms() })).map_err(trap)?;
        Ok(r.ms as u64)
    }

    fn random_u64(&mut self) -> wasmtime::Result<u64> {
        #[derive(Serialize, Deserialize)]
        struct Resp {
            v: u64,
        }
        let r = self.j.journaled("random-u64", &Empty {}, || {
            // Random bits from uuid v4 (already a dependency; no `rand` needed),
            // masked to 63 bits ON PURPOSE: values above i64::MAX lose precision
            // through sqlite3's json_extract(), which the acceptance script uses to
            // compare this value textually against the guest's output. ~62 effective
            // random bits — plenty for journaled (non-cryptographic) randomness.
            let v = (uuid::Uuid::new_v4().as_u128() as u64) & (i64::MAX as u64);
            Ok(Resp { v })
        }).map_err(trap)?;
        Ok(r.v)
    }

    fn await_event(&mut self, name: String) -> wasmtime::Result<String> {
        // Task 2.5 — external events. Same shape as durable sleep above: hand-rolled
        // replay check + park loop. The delivery step is a SINGLE transaction
        // (db::deliver_event_and_journal) — consuming the event and journaling it
        // must be atomic, or a crash between them loses (or double-delivers) it.
        #[derive(Serialize)]
        struct Req {
            name: String,
        }
        #[derive(Deserialize)]
        struct Resp {
            payload: String,
        }
        let seq = self.j.next_seq;
        self.j.next_seq += 1;
        let id = self.j.workflow_id.clone();
        let req_json =
            serde_json::to_string(&Req { name: name.clone() }).map_err(|e| trap(e.into()))?;

        // Replay path — the same verification journaled() performs.
        if let Some((rkind, rreq, rresp)) =
            db::get_journal_row(&self.j.db, &id, seq).map_err(trap)?
        {
            if rkind != "await-event" || rreq != req_json {
                return Err(trap(anyhow::anyhow!(
                    "nondeterministic replay at seq {seq}: recorded ({rkind}, {rreq}) \
                     but live code produced (await-event, {req_json}). The workflow \
                     code has diverged from its journal."
                )));
            }
            let r: Resp = serde_json::from_str(&rresp)
                .map_err(|e| trap(anyhow::Error::new(e).context("corrupt journal response")))?;
            return Ok(r.payload);
        }

        // Live path: park until a matching event arrives.
        db::set_status(&self.j.db, &id, "waiting_event", None).map_err(trap)?;
        loop {
            // Abort check BEFORE the delivering txn: an aborted worker must never
            // consume an event (SPEC.md Task 2.5).
            if self.notifier.is_aborted(&id) {
                return Err(trap(anyhow::Error::new(AbortForUpgrade)));
            }
            if let Some(payload) =
                db::deliver_event_and_journal(&mut self.j.db, &id, seq, &name, &req_json)
                    .map_err(trap)?
            {
                db::set_status(&self.j.db, &id, "running", None).map_err(trap)?;
                return Ok(payload);
            }
            self.notifier.wait(&id, std::time::Duration::from_millis(1000));
        }
    }

    fn log(&mut self, msg: String) -> wasmtime::Result<()> {
        // NOT journaled: no seq claimed, no row written (SPEC.md §4.1).
        tracing::info!(workflow = %self.j.workflow_id, "guest: {msg}");
        Ok(())
    }
}

/// Live-path HTTP GET. 30s timeout is set on the Agent (runner.rs); the body read is
/// capped at exactly 1 MiB (deterministic truncation, then lossy utf-8 so a cut
/// mid-sequence still yields a string); non-2xx maps to Err("status NNN").
///
/// Task 2.6 — retries, live path only: up to 3 attempts for transport errors
/// (including a failed body read) and status ≥500; 4xx NEVER retries (the server
/// answered; asking again won't change its mind). This function runs inside the
/// journaled() closure, so however many attempts happen here, exactly ONE journal
/// row records the final outcome — replay sees a single result.
fn do_http_get(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    // Backoff schedule per SPEC.md Task 2.6. With 3 attempts only the first two
    // gaps are reachable; the 2s slot is the schedule's next step if attempts are
    // ever raised.
    const BACKOFF_MS: [u64; 3] = [500, 1000, 2000];
    let attempts = 3;
    let mut last_err = String::new();
    for attempt in 0..attempts {
        match agent.get(url).call() {
            Ok(resp) => {
                let mut buf = Vec::new();
                match resp.into_reader().take(1024 * 1024).read_to_end(&mut buf) {
                    Ok(_) => return Ok(String::from_utf8_lossy(&buf).into_owned()),
                    // Connection died mid-body: a transport failure — retryable.
                    Err(e) => last_err = format!("read error: {e}"),
                }
            }
            Err(ureq::Error::Status(code, _)) if code < 500 => return Err(format!("status {code}")),
            Err(ureq::Error::Status(code, _)) => last_err = format!("status {code}"),
            Err(e) => last_err = format!("transport: {e}"),
        }
        if attempt + 1 < attempts {
            std::thread::sleep(std::time::Duration::from_millis(BACKOFF_MS[attempt]));
        }
    }
    Err(last_err)
}
