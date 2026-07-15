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
// PHASE 2: Task 2.1 adds await-event here; Task 2.4 REPLACES sleep_ms with a durable
// timer (park loop + `timers` table — hand-rolled, NOT via journaled()); Task 2.6
// adds retries inside do_http_get (live path only). PHASE 3: Task 3.3 adds checkpoint.

use std::io::Read;

use serde::{Deserialize, Serialize};
use wasmtime::component::bindgen;

use crate::journal::{now_ms, JournalCtx};

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
    // PHASE 2 adds: notifier handle + abort flag (SPEC.md Task 2.3).
}

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
        // PHASE 1 semantics (documented simplification): journal only on completion.
        // A crash mid-sleep re-runs the FULL sleep on recovery. Task 2.4 replaces
        // this with a durable wake_at timer + park loop; do not "improve" it here.
        #[derive(Serialize)]
        struct Req {
            ms: u64,
        }
        self.j.journaled("sleep-ms", &Req { ms }, || {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            Ok(Empty {})
        }).map_err(trap)?;
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

    fn log(&mut self, msg: String) -> wasmtime::Result<()> {
        // NOT journaled: no seq claimed, no row written (SPEC.md §4.1).
        tracing::info!(workflow = %self.j.workflow_id, "guest: {msg}");
        Ok(())
    }
}

/// Live-path HTTP GET. 30s timeout is set on the Agent (runner.rs); the body read is
/// capped at exactly 1 MiB (deterministic truncation, then lossy utf-8 so a cut
/// mid-sequence still yields a string); non-2xx maps to Err("status NNN").
/// PHASE 2 (Task 2.6): retries go HERE, on the live path only — never journal
/// intermediate attempts; replay must see exactly one row per call.
fn do_http_get(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    match agent.get(url).call() {
        Ok(resp) => {
            let mut buf = Vec::new();
            resp.into_reader()
                .take(1024 * 1024)
                .read_to_end(&mut buf)
                .map_err(|e| format!("read error: {e}"))?;
            Ok(String::from_utf8_lossy(&buf).into_owned())
        }
        Err(ureq::Error::Status(code, _)) => Err(format!("status {code}")),
        Err(e) => Err(format!("transport: {e}")),
    }
}
