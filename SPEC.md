# Build spec: "Keel" — a durable WASM workflow engine
### rev 1.1 — post-review. Fixes vs 1.0: per-connection pragmas via open_conn(); event un-delivery on upgrade (delivered_seq); abort-flag lifecycle + upgrade claim set + spawn_blocking join; axum multipart feature + 64MB module body limit; askama-without-askama_axum; max-running default 256 + starvation warning; script log redirection + de-flaked assertions; runaway-guest limitation made explicit.

A single-binary durable execution engine in Rust. It embeds wasmtime, runs user-supplied
WASM workflow components, journals every side effect to SQLite, and recovers workflows
after any crash by deterministic replay. UI is server-rendered HTML + htmx, embedded in
the binary. Built in 3 phases, each with a mandatory acceptance script.

---

## 0. Rules for the builder (read first, re-read at each phase)

1. **Do not proceed to the next phase until the current phase's acceptance script passes.**
   Run it exactly as written. If it fails, fix the engine, not the script.
2. **There is no "replay mode".** Fresh execution and crash recovery run the *same* code
   path. Recovery is simply: start the workflow from the beginning; the journal already
   contains rows, so recorded results are returned instead of re-executing effects. If you
   find yourself writing `if replaying { ... } else { ... }` anywhere in workflow
   execution, you have made an architecture error — stop and re-read §4.
3. **The journal row must be committed to SQLite BEFORE the result is returned to the
   guest.** This ordering is the entire correctness story. Never reorder it.
4. **The guest must have zero ambient capabilities.** No WASI clocks, no WASI random, no
   WASI sockets, no filesystem. The only doors to the outside world are the host functions
   in `workflow.wit`. Guests are compiled to `wasm32-unknown-unknown` to enforce this.
5. **When a version number below doesn't resolve, use the latest stable and note it.**
   Versions listed are known-good minimums as of mid-2026, not exact pins.
6. **Fragile spots are marked `FALLBACK:`** — if the primary instruction fails to compile
   or behaves differently, apply the fallback instead of improvising.
7. Commit to git at the end of every numbered task. Commit message = task number + name.

Non-goals for all 3 phases (do not build these): multi-node clustering, authentication,
TLS, HTTP methods other than GET in the guest API, streaming bodies, physical
linear-memory snapshots, WASI 0.3 async, metrics/OTel, **runaway-guest protection** (a
guest that spins in pure compute without host calls pins its thread forever and even
upgrade cannot abort it, since aborts are only observed at park points and host calls —
this is a KNOWN, ACCEPTED limitation; the production fix is wasmtime epoch interruption,
explicitly deferred. Put this warning verbatim in the README).

Two cross-cutting rules: every INSERT/UPDATE on `workflows` goes through one helper
`set_status(conn, id, status, output: Option<&str>)` in `db.rs` that also sets
`updated_at = now_ms()` — the column is NOT NULL and scattered writes will forget it.
And the only place `runner::spawn` may be called is: workflow creation, the startup
recovery scan, and step 5 of the upgrade handler — nowhere else, ever.

---

## 1. Architecture overview

One Rust binary, `keel`, containing:

- **HTTP layer** (axum on tokio): JSON API + server-rendered HTML pages (askama
  templates) + htmx for partial updates. Listens on `127.0.0.1:8080` by default.
- **Execution layer**: one OS thread per *active* workflow (`std::thread::spawn`).
  Each thread owns its own `rusqlite::Connection` and a wasmtime `Store`. Threads block
  during host calls (HTTP via `ureq`, sleeps via parking). This is deliberate: blocking
  threads keep the code simple and correct at hobby scale (hundreds of workflows).
- **Journal**: SQLite in WAL mode. The append-only `journal` table is the source of
  truth. SQLite handles multi-connection access; the engine adds a `Notifier` (in-process
  condvar registry) purely as a wake-up optimization.
- **Guests**: WASM *components* (component model, WIT-typed) built with `cargo component`,
  uploaded via the API, stored content-addressed by sha256 in the `modules` table.

Data flow for one host call (memorize this):

```
guest calls http-get(url)
  → host claims seq N (per-workflow counter, starts at 0)
  → SELECT journal row (workflow_id, N)
      → row exists: verify kind+request match recorded values
                    (mismatch = "nondeterminism" → fail workflow)
                    return recorded response          [replay path]
      → no row:    execute real HTTP GET
                    INSERT journal row (COMMIT)
                    return live response              [live path]
```

---

## 2. Repository layout

```
keel/
├── Cargo.toml                 # workspace: members = ["engine", "guests/*"]
├── wit/
│   └── workflow.wit           # the ONLY contract between engine and guests
├── engine/
│   ├── Cargo.toml
│   ├── build.rs               # (phase 2) asserts assets/htmx.min.js exists
│   ├── assets/
│   │   ├── htmx.min.js        # (phase 2) vendored, embedded via include_bytes!
│   │   └── style.css          # (phase 2)
│   ├── templates/             # (phase 2) askama templates
│   └── src/
│       ├── main.rs            # CLI (clap), startup, recovery scan
│       ├── db.rs              # schema, migrations, typed query helpers
│       ├── journal.rs         # the journaled() wrapper — heart of the engine
│       ├── host.rs            # Host trait impl: http-get, sleep-ms, now-ms, random-u64, log, (p2: await-event) (p3: checkpoint)
│       ├── runner.rs          # spawn_workflow_thread, wasmtime setup, status transitions
│       ├── notifier.rs        # (phase 2) per-workflow condvar registry + abort flags
│       ├── api.rs             # JSON endpoints
│       └── ui.rs              # (phase 2) HTML routes + partials
├── guests/
│   ├── demo/                  # phase 1 acceptance guest
│   └── counter/               # phase 3 acceptance guests (v1 and v2 via feature flag)
└── scripts/
    ├── accept_phase1.sh
    ├── accept_phase2.sh
    └── accept_phase3.sh
```

Workspace `Cargo.toml`:

```toml
[workspace]
members = ["engine"]
resolver = "2"
# guests are built separately with `cargo component`, not as workspace members,
# because they target wasm32-unknown-unknown. Keep them OUT of [members].
```

---

## 3. Toolchain and dependencies

Setup commands (run once):

```bash
rustup target add wasm32-unknown-unknown
cargo install cargo-component        # builds WASM components from Rust
sudo apt-get install -y sqlite3      # used by acceptance scripts only
```

`engine/Cargo.toml` dependencies:

```toml
[dependencies]
wasmtime = { version = "43", features = ["component-model"] }
# FALLBACK: if "component-model" is rejected as unknown, it is on by default
# in this wasmtime version — remove the features list.
tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal"] }
axum = { version = "0.8", features = ["multipart"] }   # multipart needed for the
                                                        # phase-2 module upload form
rusqlite = { version = "0.32", features = ["bundled"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
ureq = { version = "2", features = ["tls"] }
sha2 = "0.10"
hex = "0.4"
uuid = { version = "1", features = ["v4"] }
clap = { version = "4", features = ["derive"] }
askama = "0.12"          # phase 2
tracing = "0.1"
tracing-subscriber = "0.3"
```

---

## 4. The WIT contract (phase 1 version)

`wit/workflow.wit` — copy exactly:

```wit
package keel:workflow@0.1.0;

interface host-api {
    /// Journaled HTTP GET. ok = response body (utf-8, truncated by host to 1 MiB),
    /// err = human-readable error string. Non-2xx status is an err.
    http-get: func(url: string) -> result<string, string>;

    /// Journaled durable sleep.
    sleep-ms: func(ms: u64);

    /// Journaled wall-clock time, unix epoch milliseconds.
    now-ms: func() -> u64;

    /// Journaled pseudo-randomness.
    random-u64: func() -> u64;

    /// NOT journaled as an effect: forwarded to engine logs, tagged with the
    /// workflow id. Free to call; returns nothing; identical on replay by construction
    /// (host makes it a no-op when the next seq is already in the journal? NO —
    /// see §4.1: log is simply not sequence-numbered at all).
    log: func(msg: string);
}

world workflow {
    import host-api;

    /// Entry point. input/output are JSON strings (engine treats them as opaque).
    export run: func(input: string) -> result<string, string>;
}
```

### 4.1 Which host functions consume a journal sequence number

`http-get`, `sleep-ms`, `now-ms`, `random-u64` each claim exactly one seq and get exactly
one journal row. `log` claims **no** seq and writes **no** journal row — it calls
`tracing::info!` on the host and returns. (During replay the guest will call `log` again
with the same strings; duplicate log lines during recovery are expected and harmless.)

### 4.2 Journal row payload formats (JSON, exact)

| kind         | request JSON             | response JSON                                        |
|--------------|--------------------------|------------------------------------------------------|
| `http-get`   | `{"url": "..."}`         | `{"ok": "body..."}` or `{"err": "message"}`          |
| `sleep-ms`   | `{"ms": 15000}`          | `{}`                                                 |
| `now-ms`     | `{}`                     | `{"ms": 1752573600123}`                              |
| `random-u64` | `{}`                     | `{"v": 1234567890}`                                  |
| `await-event`| `{"name": "..."}` (p2)   | `{"payload": "..."}`                                 |
| `checkpoint` | `{"len": 123}` (p3)      | `{}`                                                 |

---

## 5. Database schema (phase 1 version; later phases append, never alter)

`engine/src/db.rs` MUST expose a single constructor used by every thread —
`busy_timeout` and `foreign_keys` are PER-CONNECTION in SQLite and must be applied on
every open (only WAL mode persists in the file):

```rust
pub fn open_conn(path: &str) -> Result<rusqlite::Connection> {
    let c = rusqlite::Connection::open(path)?;
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "busy_timeout", 5000)?;
    c.pragma_update(None, "foreign_keys", "ON")?;
    Ok(c)
}
```

Opening a `Connection` any other way anywhere in the codebase is a bug. Migration
(startup only, idempotent):

```sql

CREATE TABLE IF NOT EXISTS modules (
    hash        TEXT PRIMARY KEY,          -- lowercase hex sha256 of wasm bytes
    name        TEXT NOT NULL DEFAULT '',
    wasm        BLOB NOT NULL,
    created_at  INTEGER NOT NULL           -- unix millis
);

CREATE TABLE IF NOT EXISTS workflows (
    id          TEXT PRIMARY KEY,          -- uuid v4
    module_hash TEXT NOT NULL REFERENCES modules(hash),
    input       TEXT NOT NULL,             -- JSON
    status      TEXT NOT NULL,             -- 'running'|'sleeping'|'waiting_event'|'completed'|'failed'
    output      TEXT,                      -- JSON on completed; error string on failed
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS journal (
    workflow_id TEXT NOT NULL REFERENCES workflows(id),
    seq         INTEGER NOT NULL,          -- 0,1,2,... dense, per workflow
    kind        TEXT NOT NULL,
    request     TEXT NOT NULL,             -- JSON per §4.2
    response    TEXT NOT NULL,             -- JSON per §4.2
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (workflow_id, seq)
);
```

Status transitions (enforce in `runner.rs`, nowhere else):
`running → completed | failed | sleeping | waiting_event`;
`sleeping → running`; `waiting_event → running`. Terminal: `completed`, `failed`.

---

## 6. The journaling core (`journal.rs`) — implement exactly

```rust
use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};

pub struct JournalCtx {
    pub workflow_id: String,
    pub db: rusqlite::Connection,
    pub next_seq: i64,
}

impl JournalCtx {
    /// Wrap every effectful host call in this. `exec` runs ONLY on the live path.
    pub fn journaled<Req, Resp>(
        &mut self,
        kind: &str,
        req: &Req,
        exec: impl FnOnce() -> Result<Resp>,   // captures what it needs; clone cheap
                                               // handles (ureq::Agent is an Arc) into it
    ) -> Result<Resp>
    where
        Req: Serialize,
        Resp: Serialize + DeserializeOwned,
    {
        let seq = self.next_seq;
        self.next_seq += 1;
        let req_json = serde_json::to_string(req)?;

        let recorded: Option<(String, String, String)> = self
            .db
            .query_row(
                "SELECT kind, request, response FROM journal
                 WHERE workflow_id = ?1 AND seq = ?2",
                rusqlite::params![self.workflow_id, seq],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;              // use rusqlite::OptionalExtension

        if let Some((rkind, rreq, rresp)) = recorded {
            if rkind != kind || rreq != req_json {
                bail!(
                    "nondeterministic replay at seq {seq}: recorded ({rkind}, {rreq}) \
                     but live code produced ({kind}, {req_json}). The workflow code has \
                     diverged from its journal."
                );
            }
            return serde_json::from_str(&rresp).context("corrupt journal response");
        }

        let resp = exec()?;                          // real side effect happens here
        let resp_json = serde_json::to_string(&resp)?;
        self.db.execute(
            "INSERT INTO journal (workflow_id, seq, kind, request, response, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![self.workflow_id, seq, kind, req_json, resp_json, now_ms()],
        )?;                                          // committed BEFORE returning
        Ok(resp)
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
```

Note the phase 1 at-least-once caveat, verbatim, as a comment above `exec`: *a crash after
`exec` succeeds but before the INSERT commits means the effect re-runs on recovery. This
is accepted for phase 1–3; mitigation (intent records + idempotency keys) is out of scope.*

---

# PHASE 1 — Core durable execution (the kill -9 milestone)

Goal: submit a workflow over HTTP, `kill -9` the engine mid-run, restart it, and watch the
workflow complete without re-executing already-journaled effects.

## Task 1.1 — Engine skeleton and DB

`main.rs`: clap CLI with one subcommand:

```
keel serve [--db keel.db] [--listen 127.0.0.1:8080]
```

`serve` does, in order: init tracing-subscriber; open DB + run schema from §5; run the
**recovery scan** (Task 1.4); start axum. Handle Ctrl-C via `tokio::signal` for a clean
exit (workflow threads are detached; abrupt exit is always safe by design — that is the
whole point of the journal).

## Task 1.2 — wasmtime host bindings (`host.rs`)

```rust
use wasmtime::component::bindgen;

bindgen!({
    path: "../wit",          // directory containing workflow.wit
    world: "workflow",
});
// FALLBACK: if the macro can't find the wit dir, use an absolute-from-crate path
// literal "wit" copied into engine/wit/, and keep the two copies in sync.
```

This generates (names may differ slightly by wasmtime version — check `cargo doc` output
if they do; that is the sanctioned way to resolve mismatches):

- a `Workflow` struct with `Workflow::instantiate(&mut store, &component, &linker)` and
  `.call_run(&mut store, input) -> wasmtime::Result<Result<String, String>>`
- a host trait at `keel::workflow::host_api::Host` to implement on your store data.

Store data type and trait impl:

```rust
pub struct Ctx {
    pub j: JournalCtx,
    pub http: ureq::Agent,
    // phase 2 adds: notifier handle, abort flag
}

impl keel::workflow::host_api::Host for Ctx {
    fn http_get(&mut self, url: String) -> wasmtime::Result<Result<String, String>> {
        #[derive(serde::Serialize)] struct Req { url: String }
        #[derive(serde::Serialize, serde::Deserialize)]
        #[serde(untagged)] enum Resp { Ok { ok: String }, Err { err: String } }

        let agent = self.http.clone();               // Agent is Arc-backed: cheap clone,
        let u = url.clone();                          // avoids borrowing self inside the
        let r = self.j.journaled("http-get", &Req { url }, move || {
            Ok(match do_http_get(&agent, &u) {        // closure while self.j is &mut
                Ok(body) => Resp::Ok { ok: body },
                Err(e)   => Resp::Err { err: e },
            })
        })?;
        Ok(match r { Resp::Ok { ok } => Ok(ok), Resp::Err { err } => Err(err) })
    }

    fn sleep_ms(&mut self, ms: u64) -> wasmtime::Result<()> {
        // PHASE 1 semantics (documented simplification): journal only on completion.
        // A crash mid-sleep re-runs the full sleep on recovery. Phase 2 replaces this.
        #[derive(serde::Serialize)] struct Req { ms: u64 }
        #[derive(serde::Serialize, serde::Deserialize)] struct Empty {}
        self.j.journaled("sleep-ms", &Req { ms }, |_| {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            Ok(Empty {})
        })?;
        Ok(())
    }

    fn now_ms(&mut self) -> wasmtime::Result<u64> { /* journaled("now-ms", ...) */ }
    fn random_u64(&mut self) -> wasmtime::Result<u64> { /* journaled("random-u64", ...), use rand or hash of uuid */ }
    fn log(&mut self, msg: String) -> wasmtime::Result<()> {
        tracing::info!(workflow = %self.j.workflow_id, "guest: {msg}"); Ok(())
    }
}
```

`do_http_get`: `ureq` GET with 30s timeout; read body up to 1 MiB; treat non-2xx as Err
with `format!("status {code}")`. Deterministic truncation: always cut at exactly 1 MiB.

wasmtime `Engine` config: defaults, plus `config.wasm_component_model(true)` if not
already default. Create ONE `Engine` at startup in an `Arc`; a fresh `Store<Ctx>` per
workflow run. Cache compiled `Component`s in `Mutex<HashMap<String /*hash*/, Component>>`.

## Task 1.3 — Runner (`runner.rs`)

`pub fn spawn(engine: Arc<EngineShared>, workflow_id: String)` →
`std::thread::spawn(move || { ... })`:

1. Open a fresh `rusqlite::Connection` (each thread owns its own; never share).
2. Load workflow row; load module blob by hash; get-or-compile `Component`.
3. Build `Store<Ctx>` with `next_seq = 0` (ALWAYS 0 — recovery is not special).
4. `Linker::new`, `Workflow::add_to_linker(&mut linker, |c: &mut Ctx| c)`, instantiate.
5. Set status `running`, then `call_run(input)`.
6. On `Ok(Ok(json))` → status `completed`, output = json.
   On `Ok(Err(apperr))` → status `failed`, output = apperr.
   On `Err(trap)` → status `failed`, output = `format!("trap: {trap:#}")`.
   (Phase 2 adds a suspend/abort sentinel check here.)

## Task 1.4 — Recovery scan

At startup, before axum binds:
`SELECT id FROM workflows WHERE status IN ('running','sleeping','waiting_event')`
→ `runner::spawn` each. That's the entire recovery implementation. Log one line per
recovered workflow: `recovering workflow <id>`.

## Task 1.5 — JSON API (`api.rs`)

| Method & path                     | Body / response                                              |
|-----------------------------------|--------------------------------------------------------------|
| `POST /api/modules?name=demo`     | body = raw wasm bytes → `{"hash": "<sha256hex>"}` (INSERT OR IGNORE). Apply `DefaultBodyLimit::max(64 * 1024 * 1024)` to this route — axum's ~2MB default rejects real components |
| `POST /api/workflows`             | `{"module_hash": "...", "input": {...any json...}}` → `{"id": "..."}`; stores input as string, then `runner::spawn` |
| `GET  /api/workflows/:id`         | `{"id","status","output","module_hash","created_at","updated_at"}` |
| `GET  /api/workflows/:id/journal` | `[{"seq","kind","request","response","created_at"}, ...]`    |

Errors: 404 unknown id/hash; 400 malformed JSON. Plain `serde_json` maps; no extra types.

## Task 1.6 — Demo guest (`guests/demo`)

Create with `cargo component new demo --lib`, then set in `guests/demo/Cargo.toml`:

```toml
[package.metadata.component]
package = "keel:demo"

[package.metadata.component.target]
path = "../../wit"
world = "workflow"
```

`src/lib.rs` (cargo-component generates a `bindings` module exposing the world; implement
the `Guest` trait it defines):

```rust
#[allow(warnings)] mod bindings;
use bindings::keel::workflow::host_api as host;

struct Component;
impl bindings::Guest for Component {
    fn run(input: String) -> Result<String, String> {
        host::log(&format!("starting with input {input}"));
        let stamp = host::random_u64();
        let a = host::http_get("https://example.com/")?;
        host::log(&format!("first fetch: {} bytes", a.len()));
        host::sleep_ms(15_000);
        let b = host::http_get("https://example.com/")?;
        Ok(format!(
            r#"{{"stamp":{stamp},"first_len":{},"second_len":{}}}"#, a.len(), b.len()
        ))
    }
}
bindings::export!(Component with_types_in bindings);
```

Build: `cargo component build --release --target wasm32-unknown-unknown`
→ `guests/demo/target/wasm32-unknown-unknown/release/demo.wasm` (a component).

Guest rules (put in a comment at the top of every guest): no `std::time`, no
`std::thread::sleep`, no `println!`, no `rand` — those either panic or are silently
useless on wasm32-unknown-unknown; use `host::*` instead. That restriction is a feature:
it is what makes the guest deterministic.

FALLBACK: if instantiation fails at runtime with unresolved `wasi:*` imports, the guest
was built for the default wasip1 target — rebuild with the explicit
`--target wasm32-unknown-unknown`.

## Task 1.7 — Acceptance (`scripts/accept_phase1.sh`)

```bash
#!/usr/bin/env bash
set -euo pipefail
DB=accept1.db; rm -f $DB $DB-*
cargo build --release -p keel-engine
(cd guests/demo && cargo component build --release --target wasm32-unknown-unknown)

./target/release/keel serve --db $DB > engine.log 2>&1 & ENG=$!; sleep 1
# ALL acceptance scripts redirect engine output to engine.log (appending on restart:
# use >> for the second launch) — phase 3's script greps it for "resuming".

HASH=$(curl -s -X POST --data-binary @guests/demo/target/wasm32-unknown-unknown/release/demo.wasm \
  "localhost:8080/api/modules?name=demo" | python3 -c 'import sys,json;print(json.load(sys.stdin)["hash"])')
WF=$(curl -s -X POST localhost:8080/api/workflows \
  -H 'content-type: application/json' \
  -d "{\"module_hash\":\"$HASH\",\"input\":{}}" | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')

sleep 5                                   # inside the 15s guest sleep by now
kill -9 $ENG                              # ungraceful, mid-workflow
sleep 1
./target/release/keel serve --db $DB >> engine.log 2>&1 & ENG=$!

for i in $(seq 1 40); do
  ST=$(curl -s localhost:8080/api/workflows/$WF | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
  [ "$ST" = "completed" ] && break; sleep 1
done
kill $ENG || true
[ "$ST" = "completed" ] || { echo "FAIL: status=$ST"; exit 1; }

# exactly 2 http-gets total: the pre-crash one was NOT re-executed
N=$(sqlite3 $DB "SELECT COUNT(*) FROM journal WHERE workflow_id='$WF' AND kind='http-get'")
[ "$N" = "2" ] || { echo "FAIL: expected 2 http-get rows, got $N"; exit 1; }

# the replayed random matches the recorded one → output stamp == journal row.
# NOTE: check the DB's output column, NOT the API response — the API returns output as
# an escaped JSON string, so grepping the API body for "stamp": is dead on arrival.
STAMP=$(sqlite3 $DB "SELECT json_extract(response,'\$.v') FROM journal WHERE workflow_id='$WF' AND kind='random-u64'")
sqlite3 $DB "SELECT output FROM workflows WHERE id='$WF'" | grep -q "\"stamp\":$STAMP" \
  || { echo "FAIL: replayed random diverged"; exit 1; }
echo "PHASE 1 PASS"
```

Definition of done: script prints `PHASE 1 PASS` twice in a row from a clean checkout.

---

# PHASE 2 — Many workflows, durable timers, external events, htmx UI

Prereq: Phase 1 acceptance green.

## Task 2.1 — WIT addition (backwards compatible)

Bump `wit/workflow.wit` to `package keel:workflow@0.2.0` and add ONE function to
`host-api`:

```wit
    /// Journaled. Blocks until an event with this name is delivered to the workflow
    /// via POST /api/workflows/:id/events. Returns the event payload (JSON string).
    await-event: func(name: string) -> string;
```

Adding an *import* is non-breaking: phase-1 guests simply don't import it and keep
working. Rebuild bindings (`cargo build` regenerates from the macro).

## Task 2.2 — Schema additions (append to migration; never ALTER)

```sql
CREATE TABLE IF NOT EXISTS timers (
    workflow_id TEXT PRIMARY KEY REFERENCES workflows(id),
    seq         INTEGER NOT NULL,
    wake_at     INTEGER NOT NULL            -- unix millis; FIXED once written
);
CREATE TABLE IF NOT EXISTS events (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    workflow_id   TEXT NOT NULL REFERENCES workflows(id),
    name          TEXT NOT NULL,
    payload       TEXT NOT NULL,
    delivered     INTEGER NOT NULL DEFAULT 0,
    delivered_seq INTEGER,                  -- journal seq that consumed this event;
                                            -- REQUIRED so a phase-3 upgrade can
                                            -- un-deliver events whose journal rows
                                            -- fall in the discarded tail
    created_at    INTEGER NOT NULL
);
```

## Task 2.3 — Notifier (`notifier.rs`)

In-process wake-ups so parked threads don't rely solely on 1s polling:

```rust
pub struct Notifier { /* Mutex<HashMap<String, Arc<(Mutex<u64>, Condvar)>>> + Mutex<HashSet<String>> abort set */ }
impl Notifier {
    pub fn wait(&self, id: &str, timeout: Duration);   // condvar wait_timeout; spurious wakes fine
    pub fn notify(&self, id: &str);
    pub fn set_abort(&self, id: &str);                 // phase 3 uses this
    pub fn is_aborted(&self, id: &str) -> bool;
    pub fn clear_abort(&self, id: &str);
}
```

`wait(id)` and `notify(id)` MUST get-or-insert the map entry (a notify before the first
wait must not be lost into a missing key). Durability NEVER depends on the Notifier —
every park loop also has a 1-second `wait_timeout` and re-checks the database. The
Notifier only reduces latency.

## Task 2.4 — Durable sleep (replaces phase 1 `sleep_ms`)

Hand-rolled (mirrors `journaled()` but with a park loop; do not force it through the
generic wrapper):

```
claim seq (next_seq++, as usual)
SELECT journal row (id, seq) → exists? verify kind/request, return.   [replay done]
SELECT timers row for workflow:
   none → INSERT (id, seq, wake_at = now_ms() + ms)                    [first arrival]
   some → keep its wake_at                                             [restart mid-sleep]
UPDATE workflows SET status='sleeping'
loop:
   if notifier.is_aborted(id) → bail!(AbortForUpgrade)                 [phase 3]
   remaining = wake_at - now_ms(); if remaining <= 0 → break
   notifier.wait(id, min(remaining, 1000ms))
if notifier.is_aborted(id) → bail!(AbortForUpgrade)     // final re-check: never commit
                                                        // completion after an abort
DELETE timers row; INSERT journal row ("sleep-ms", {"ms":ms}, {}); status='running'; return
```

Key property to verify by hand: kill -9 during the sleep, restart → replay reaches the
same seq, finds the SAME `wake_at`, sleeps only the remainder. The phase-1 "full re-sleep"
wart is now gone.

## Task 2.5 — External events

Host `await_event(name)`:

```
claim seq; journal row exists? → return recorded payload.
UPDATE status='waiting_event'
loop:
   if is_aborted → bail!(AbortForUpgrade)      // checked BEFORE the delivering txn, so
                                               // an aborted worker never consumes an event
   in ONE transaction:
     SELECT oldest events row WHERE workflow_id=? AND name=? AND delivered=0
     if found: UPDATE delivered=1, delivered_seq=<this call's seq>;
               INSERT journal ("await-event", {"name":..}, {"payload":..}); COMMIT
   if found → status='running'; return payload
   notifier.wait(id, 1000ms)
```

The single transaction is mandatory: marking delivered and journaling must be atomic, or
a crash between them silently loses the event.

API: `POST /api/workflows/:id/events` body `{"name":"approve","payload":{...}}` →
INSERT events row (payload stored as JSON string) → `notifier.notify(id)` → 202.

## Task 2.6 — HTTP retries (live path only)

In `do_http_get`: up to 3 attempts; retry on transport errors and status ≥500; never on
4xx. Backoff 500ms, 1s, 2s. Only the final outcome is journaled (replay sees one row).

## Task 2.7 — Concurrency cap

`--max-running N` (default **256** — std has no Semaphore; build the counter from
`Arc<(Mutex<u32>, Condvar)>`) acquired at thread start, released on exit. WARNING to
print at startup when >80% of permits are held: parked (sleeping/waiting) threads still
hold permits in this design, so N parked workflows starve recovery of workflow N+1
after a restart. Parked OS threads are cheap; keep N generously above your workflow
count. The thread-free-parking fix is deliberately out of scope.

## Task 2.8 — htmx UI (`ui.rs`, `templates/`, `assets/`)

Templating: do NOT add `askama_axum` (version-coupling churn). Render templates to
`String` and return `axum::response::Html(s)` — that is the whole integration.

Vendor htmx once: download `https://unpkg.com/htmx.org@2/dist/htmx.min.js` into
`engine/assets/htmx.min.js` and COMMIT it. Add a 3-line `build.rs` that panics with a
clear message if `assets/htmx.min.js` is missing (fail at build, not at runtime). Serve
embedded:
`include_bytes!` → `GET /assets/htmx.min.js` (`text/javascript`) and
`GET /assets/style.css` (`text/css`). No CDN references in templates — the binary stays
self-contained offline.

Routes (askama templates; htmx for polling partials):

| Route | Template | Contents |
|---|---|---|
| `GET /` | `dashboard.html` | table of workflows via partial below |
| `GET /partials/workflows` | `_workflows_table.html` | `<tbody>` rows: short id (link), module name, status badge, updated; polled with `hx-get="/partials/workflows" hx-trigger="every 2s" hx-swap="innerHTML"` |
| `GET /workflows/:id` | `workflow.html` | header + polled detail partial + "Send event" form (`hx-post`, fields: name, payload) |
| `GET /partials/workflows/:id` | `_workflow_detail.html` | status badge, input, output, journal table (seq, kind, request, response, time) |
| `GET /modules` | `modules.html` | upload form (file + name, posts to `/api/modules` via normal multipart→ accept both raw and multipart) + module list + "Start workflow" form (module select, JSON textarea) |

Copy rules (apply everywhere): sentence case; buttons say what they do — "Start
workflow", "Send event", "Upload module", never "Submit"; empty states instruct ("No
workflows yet — upload a module to start one."); errors state cause + fix, no apologies.

Visual identity — small, exact, done (`style.css`, ~50 lines, no framework): terminal-ledger
aesthetic. `font-family: ui-monospace, 'JetBrains Mono', monospace` throughout;
background `#faf9f5`; ink `#141413`; hairline table rules `1px solid #e0ded6`; one accent
`#0f6e56` (links, focus rings, primary buttons). Status badges = 2px-radius pills:
running `#854f0b/#faeeda`, sleeping `#185fa5/#e6f1fb`, waiting_event `#534ab7/#eeedfe`,
completed `#3b6d11/#eaf3de`, failed `#a32d2d/#fcebeb` (text/bg). `:focus-visible`
outline 2px accent. Max content width 960px. Nothing else — resist additions.

## Task 2.9 — Approval guest (`guests/approval`)

Same crate setup as demo. Logic: `log`; `let d = await-event("approve")`;
`sleep-ms(60_000)`; return `{"approved_with": <d>}`-style JSON.

## Task 2.10 — Acceptance (`scripts/accept_phase2.sh`)

Fresh DB. Steps and assertions (write the script in the same style as phase 1):

1. Build engine + approval guest; start engine; upload; start workflow.
2. Poll until status = `waiting_event`. kill -9. Restart. **Poll (up to 15s)** until
   status = `waiting_event` again — recovery legitimately passes through `running` for
   a moment while replaying, so an immediate assert is a flake, not a check.
3. `POST .../events {"name":"approve","payload":{"by":"alice"}}`. Poll until status =
   `sleeping`. Record `W1 = SELECT wake_at FROM timers WHERE workflow_id=...`.
4. kill -9 mid-sleep. Restart. Record `W2` the same way. **Assert W1 == W2** (remaining-
   time sleep, not restarted sleep).
5. Poll until `completed` (allow 90s). Assert output contains `alice`. Assert
   `COUNT(journal WHERE kind='await-event') == 1`.
6. UI smoke: `curl -s localhost:8080/` contains `Workflows`; `curl -s /workflows/$WF`
   contains the id; `curl -s /assets/htmx.min.js | head -c 100` is nonempty.

Definition of done: `PHASE 2 PASS`, twice in a row, fresh DB each time.

---

# PHASE 3 — Logical checkpoints, journal pruning, live code upgrade

Prereq: Phase 2 green. **This phase changes the WIT world (adds an export) — it is a
breaking change for guests.** All guests must be rebuilt; acceptance uses a fresh DB.

## Task 3.1 — WIT 0.3.0

```wit
package keel:workflow@0.3.0;

interface host-api {
    // ... everything from 0.2.0 unchanged ...

    /// Journaled. The guest hands the engine a self-serialized state blob at a safe
    /// point. Contract: state must fully determine all FUTURE host calls the guest
    /// will make (given replayed results). The engine may later restart the guest
    /// via resume(state) instead of run(input).
    checkpoint: func(state: list<u8>);
}

world workflow {
    import host-api;
    export run: func(input: string) -> result<string, string>;
    /// Continue from a checkpointed state blob. Guests that never call checkpoint
    /// implement this as: return Err("no checkpoints").
    export resume: func(state: list<u8>) -> result<string, string>;
}
```

Update `guests/demo` and `guests/approval` with the stub `resume`.

## Task 3.2 — Schema addition

```sql
CREATE TABLE IF NOT EXISTS snapshots (
    workflow_id TEXT PRIMARY KEY REFERENCES workflows(id),
    journal_seq INTEGER NOT NULL,   -- the checkpoint call's own seq (call it C)
    state       BLOB NOT NULL,
    module_hash TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
```

## Task 3.3 — `checkpoint` host implementation

Goes through `journaled("checkpoint", {"len": state.len()}, exec)` where `exec`, in ONE
transaction: `INSERT OR REPLACE snapshots (id, C, state, current module_hash, now)` then
`DELETE FROM journal WHERE workflow_id=? AND seq < C` (strictly less — pruning!). The
wrapper then inserts journal row C as usual. Invariant after a checkpoint at C: journal
contains exactly row C plus any rows > C.

## Task 3.4 — Recovery via resume

In `runner.rs`, replace step 3–5 with:

```
match SELECT * FROM snapshots WHERE workflow_id = ?:
  None       → next_seq = 0;                    call_run(input)
  Some(snap) → assert snap.module_hash == workflows.module_hash
               (mismatch → status 'failed' with explanatory output);
               next_seq = snap.journal_seq + 1;  call_resume(snap.state)
```

Log `resuming <id> from checkpoint seq <C>` — the acceptance script greps for it. Note
this is still not a "replay mode": rows > C replay through the identical journaled()
path.

## Task 3.5 — Thread registry + abort sentinel

- `runner.rs` keeps `Mutex<HashMap<String, JoinHandle<()>>>`; inserted on spawn, removed
  by the thread itself on exit.
- Define `#[derive(Debug)] struct AbortForUpgrade;` (impl Display + std::error::Error).
  Park loops (Tasks 2.4/2.5) already bail with it when `is_aborted`.
- In the runner's result match, FIRST check
  `err.root_cause().downcast_ref::<AbortForUpgrade>().is_some()`
  (also try `err.chain()` if root_cause misses — FALLBACK: walk the chain). If aborted:
  clear the abort flag, leave `status` untouched, exit the thread silently. Only
  otherwise mark `failed`.

## Task 3.6 — Upgrade endpoint

`POST /api/workflows/:id/upgrade` body `{"module_hash": "<new>"}`:

1. Claim: insert the id into an in-process `Mutex<HashSet<String>>` of in-flight
   upgrades; if already present → 409 "upgrade already in progress". Remove the id from
   the set on EVERY exit path of this handler (success and failure) — use a guard.
2. Validate: workflow exists; new hash exists in `modules`; status ∈
   {`sleeping`,`waiting_event`}; snapshot row exists. Else 409 with a one-line reason.
3. `notifier.set_abort(id); notifier.notify(id);` remove the JoinHandle from the
   registry. `None` means the thread already exited — proceed. `Some(h)` → join it
   inside `tokio::task::spawn_blocking` (NEVER a bare `join()` in an async handler),
   bounded at 30s. On timeout: **`notifier.clear_abort(id)`**, then 409 "workflow is
   actively executing (not parked); retry when it parks". Leaving the abort flag set
   here would zombie the workflow at its next park — clearing it is mandatory.
4. In ONE transaction: `DELETE FROM journal WHERE workflow_id=? AND seq > C` (C =
   snapshot journal_seq); `DELETE FROM timers WHERE workflow_id=?`;
   `UPDATE events SET delivered=0, delivered_seq=NULL WHERE workflow_id=? AND
   delivered_seq > C` (un-deliver events consumed by the discarded tail — without this,
   a re-executed await-event waits forever for an event that was already consumed);
   `UPDATE workflows SET module_hash=new`; `UPDATE snapshots SET module_hash=new`.
   (Undelivered `events` rows are untouched and remain deliverable.)
5. `runner::spawn(id)` → resumes under new code. Document the one behavioral wrinkle in
   the README: an in-flight sleep restarts from `resume` with a fresh full duration
   after upgrade, because its timer + journal tail were discarded.
6. UI: on the workflow detail page, when status is sleeping/waiting_event and a snapshot
   exists, show module select + "Upgrade module" button (hx-post to the endpoint).

## Task 3.7 — Counter guests (v1/v2 in one crate via feature flag)

`guests/counter`, feature `v2`. Shared shape: `run(input)` parses `{"target": u32}`,
initializes state, enters `tick_loop(state)`; `resume(bytes)` parses state, enters the
same `tick_loop`.

- v1 state: `{"count": u32, "target": u32}`. Loop: while `count < target`:
  `sleep-ms(5000)`; `count += 1`; `checkpoint(serde_json bytes)`. Return
  `{"count": count}`.
- v2 state: `{"total": u32, "target": u32, "note": String}`. `resume` FIRST tries v2
  shape, on parse failure parses v1 and maps `{total: count, note: "upgraded"}`. Return
  `{"total": total, "note": note}`.

Build both artifacts:
`cargo component build --release --target wasm32-unknown-unknown` and again with
`--features v2` (copy each .wasm aside before the second build — same output filename).

## Task 3.8 — Acceptance (`scripts/accept_phase3.sh`)

Fresh DB. Assertions:

1. Upload counter-v1 and counter-v2 (two distinct hashes). Start v1 with
   `{"target": 8}`.
2. After ~12s, assert a `snapshots` row exists AND
   `SELECT COUNT(*) FROM journal WHERE workflow_id=?` ≤ 4 (pruning is working; without
   it the count would grow by ~2 per tick).
3. kill -9 mid-sleep; restart; grep engine log for `resuming` (resume path used, not
   full replay).
4. Poll until status = `sleeping` again, then call upgrade with the v2 hash → expect 200.
5. Poll until `completed` (allow 120s). Assert output contains `"note":"upgraded"` and
   `"total":8`. Assert `workflows.module_hash` == v2 hash.
6. Negative check: upgrading a `completed` workflow returns 409.

Definition of done: `PHASE 3 PASS` twice in a row, fresh DB each time.

---

# Troubleshooting table (check here before improvising)

| Symptom | Likely cause | Fix |
|---|---|---|
| `bindgen!` compile errors about world/paths | wit path relative to wrong dir | use `path: "wit"` with a copy at `engine/wit/`, keep in sync with root `wit/` |
| Instantiation error: missing `wasi:cli/...` imports | guest built for wasip1 | rebuild guest with `--target wasm32-unknown-unknown` |
| Guest panics on `SystemTime`/`println!` | guest used std facilities | route through `host::now-ms` / `host::log` |
| `database is locked` | a Connection opened without `db::open_conn()` (per-connection pragmas skipped) | route every open through `open_conn()`; one Connection per thread |
| Workflow marked failed with "nondeterministic replay at seq N" | guest code changed under an in-flight workflow, or guest branches on non-journaled data | never mutate uploaded modules (content-addressing prevents it); audit guest for std time/random |
| Upgrade returns 409 "actively executing" repeatedly | park loop missing an `is_aborted` check, or worker mid live http call | abort checks belong in BOTH park loops AND as the final pre-completion re-check; otherwise just retry when parked |
| Workflow stuck in `sleeping` with no thread after a failed upgrade | abort flag left set on the 409/timeout path | the upgrade handler MUST `clear_abort` on every failure exit (Task 3.6 step 3) |
| Duplicate log lines after restart | none — expected | replay re-executes `log`; it is not journaled |

# Execution notes for the builder

Work strictly in task order; each task compiles (`cargo build`) before moving on. The
acceptance scripts are the source of truth for "done" — do not weaken an assertion to
make it pass. Do not refactor earlier phases while building later ones except where a
task explicitly says "replaces". Keep every SQL statement in `db.rs` so the schema story
stays auditable in one file. When wasmtime's generated names differ from this spec,
trust `cargo doc --open` over the spec and leave a code comment noting the difference.
