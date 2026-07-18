# Keel v2 — micro-cloud extension (phases 4–6)
### rev 1.0 — extends `durable-engine-build-spec.md` rev 1.1. Do not start until base phases 1–3 are green.

Keel grows from a durable workflow engine into a single-binary micro-cloud:

- **Phase 4 — Functions.** Stateless serverless: upload a component, bind it to a URL
  prefix, get a fresh sandboxed instance per request. Functions can start and query
  durable workflows — the Lambda + Step Functions merger, in one process.
- **Phase 5 — Sandbox, metering, playground.** Fuel + epoch + memory limits, a
  per-invocation usage ledger, and a LeetCode-style judge that runs untrusted solver
  components against test cases with verdicts (AC / WA / TLE / MLE / RE).
- **Phase 6 — Hosted apps.** The platform serves full-stack apps: a Rust→WASM frontend
  (Leptos, client-side rendered) as static assets plus a backend function, uploaded as
  one zip. Acceptance is a vertical slice: browser-served WASM UI → function → durable
  workflow → result, one binary end to end.

All rules from the base spec §0 still apply (acceptance gates, no replay mode,
journal-commit-before-return, `open_conn()` for every connection, `set_status()` for
every workflows write, git commit per task). Where this file conflicts with the base
spec, **this file governs** — notably: phase 5 SUPERSEDES the base spec's
"runaway-guest protection is a non-goal": from phase 5 on, runaway guests are killed by
epochs, including workflow guests.

Updated non-goals (phases 4–6): auth/multi-tenancy (this is a single-operator
platform), TLS, streaming request/response bodies (buffer, 10 MiB cap), in-browser
compilation (a code-to-wasm compile service is a separate machine's job; the playground
accepts prebuilt components), `wasi:http/proxy` compatibility (see Stretch section —
deliberately deferred so the ecosystem crates don't destabilize the build), physical
memory snapshots (still deferred, per base spec).

---

## E1. Design decisions the builder must not "improve"

**One wasmtime Engine, two limit profiles.** The Engine config enables BOTH
`consume_fuel(true)` and `epoch_interruption(true)`. What differs is per-Store setup:

```
workflow stores:  set_fuel(cfg.wf_fuel_limit)      // default 10^13 — a runaway
                  set_epoch_deadline(10^12 ticks)  //   kill-switch, not a quota
                  epoch_deadline_trap()            // deadline ≈ millennia: epochs
                                                   //   intentionally never fire here
function/solver:  set_fuel(per-request limit)      // real quotas, both dimensions
                  set_epoch_deadline(ceil(ms/100))
                  epoch_deadline_trap()
```

Why fuel (not epochs) guards workflows: fuel is consumed ONLY by executing guest
instructions, so a workflow parked in a sleep or await for 30 days spends zero — the
watchdog cannot false-positive on parks. And fuel consumption is deterministic (same
instructions → same fuel), so with the budget reset to the full `wf_fuel_limit` at
every `call_run`/`call_resume`, replay of any segment consumes exactly what the
original did and never trips a limit the original survived. 10^13 instructions of
headroom is minutes of continuous compute — an infinite loop dies, a long replay does
not (and base phase-3 checkpoints keep replay cost bounded anyway).

Yes, `consume_fuel(true)` adds per-instruction overhead to workflow guests too. For
I/O-shaped workflow code this is noise, and it buys one engine, one component cache
(`Mutex<HashMap<hash, Component>>`), and one compilation per module. Do not "optimize"
back to two Engines.

**One epoch ticker thread** (spawned in `serve` before axum binds, never exits): every
100ms call `engine.increment_epoch()`. All deadlines are measured in these 100ms ticks.

**Workflow runaway protection (retrofit, three lines):** in the base runner, after
Store creation and before `call_run`/`call_resume`, apply the workflow profile from the
block above: `set_fuel(cfg.wf_fuel_limit)` (new CLI flag `--wf-fuel-limit`, default
10_000_000_000_000), plus the effectively-infinite epoch deadline + trap. A
`Trap::OutOfFuel` from a workflow → status `failed`, output
`"runaway guest: exhausted compute budget"`. This SUPERSEDES the base spec's
runaway-guest non-goal, and it removes the corresponding README warning. Note what it
does NOT do: it cannot interrupt a park (parks are host-side; they're already
interruptible via the abort mechanism) and it cannot impose wall-clock limits on
workflows — by design, since workflows legitimately live for months.
FALLBACK: if `epoch_deadline_trap` isn't the method name in your wasmtime, find the
Store method that makes epoch expiry trap (vs yield) via `cargo doc`.

**Functions use a custom `handler` world, not `wasi:http/proxy`.** Rationale: proxy
world drags in `wasi:io` streams and the `wasmtime-wasi-http` crate — real ecosystem
compatibility, but a materially larger integration surface. The custom world reuses the
exact toolchain the builder already knows from phases 1–3 (cargo-component,
wasm32-unknown-unknown, zero ambient WASI). Compatibility mode is Stretch, not core.

**Functions are NOT durable and NOT deterministic — on purpose.** No journal, no
replay, direct `now`/`random`. Durability lives one door over: a function that needs
reliability calls `start-workflow`. Never let journal code leak into the function path.

**SQLite access on the request path:** function invocations run inside
`tokio::task::spawn_blocking`; each invocation opens its own connection via
`db::open_conn()` (base spec) — never a bare `Connection::open`, never a shared
connection across threads. If profiling later shows open cost matters, a pool is an
allowed optimization, not a phase requirement.

---

## E2. WIT 0.4.0 (breaking bump; rebuild ALL guests; wipe dev DBs)

`wit/workflow.wit` becomes `package keel:workflow@0.4.0` containing everything from
0.3.0 unchanged PLUS:

```wit
interface platform-api {
    /// Direct (NOT journaled — functions are not durable).
    log: func(msg: string);
    now-ms: func() -> u64;
    random-u64: func() -> u64;

    /// Start a durable workflow (base-spec semantics). Returns workflow id.
    start-workflow: func(module-hash: string, input: string) -> result<string, string>;

    /// Fetch workflow status as a JSON string:
    /// {"id","status","output"} — same shape as GET /api/workflows/:id.
    get-workflow: func(id: string) -> result<string, string>;
}

record http-request {
    method: string,          // "GET", "POST", ...
    path: string,            // path AFTER the route prefix, always starts with "/"
    query: string,           // raw query string, may be ""
    headers: list<tuple<string, string>>,
    body: list<u8>,          // host caps at 10 MiB before invoking
}

record http-response {
    status: u16,
    headers: list<tuple<string, string>>,
    body: list<u8>,
}

world handler {
    import platform-api;
    export handle: func(req: http-request) -> http-response;
}

world solver {
    // NO imports at all: solvers are pure compute. This is the tightest sandbox
    // in the platform — the module cannot name a single external capability.
    export solve: func(input: string) -> result<string, string>;
}
```

Engine side: THREE `bindgen!` invocations now (worlds `workflow`, `handler`, `solver`),
each in its own Rust module to avoid name collisions
(`mod wf_bindings { bindgen!({... world: "workflow"}) }` etc.).
FALLBACK: if bindgen collides on shared types, add `with:`/`ownership` options per
wasmtime docs, or as last resort duplicate the records into a second .wit package —
correctness over elegance.

Guest crates select their world in `Cargo.toml`
(`[package.metadata.component.target] world = "handler"` etc.), same pattern as base.

---

## E3. Schema additions (append to migration)

```sql
CREATE TABLE IF NOT EXISTS routes (
    prefix       TEXT PRIMARY KEY,      -- e.g. '/fn/echo'; longest-prefix match wins
    module_hash  TEXT NOT NULL REFERENCES modules(hash),
    fuel_limit   INTEGER NOT NULL DEFAULT 500000000,   -- 5e8 ≈ generous
    mem_limit    INTEGER NOT NULL DEFAULT 67108864,    -- 64 MiB
    time_limit_ms INTEGER NOT NULL DEFAULT 5000,
    created_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS invocations (              -- the usage ledger
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL,          -- 'function' | 'solve'
    ref         TEXT NOT NULL,          -- route prefix, or submission id
    module_hash TEXT NOT NULL,
    outcome     TEXT NOT NULL,          -- 'ok'|'guest_error'|'tle'|'mle'|'oof'|'trap'
    fuel_used   INTEGER,
    peak_mem    INTEGER,
    duration_ms INTEGER NOT NULL,
    created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS problems (
    slug        TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    statement   TEXT NOT NULL           -- markdown, rendered as <pre> (no md dep)
);
CREATE TABLE IF NOT EXISTS cases (
    problem     TEXT NOT NULL REFERENCES problems(slug),
    idx         INTEGER NOT NULL,
    input       TEXT NOT NULL,
    expected    TEXT NOT NULL,          -- exact string match after trim
    PRIMARY KEY (problem, idx)
);
CREATE TABLE IF NOT EXISTS submissions (
    id          TEXT PRIMARY KEY,       -- uuid
    problem     TEXT NOT NULL REFERENCES problems(slug),
    module_hash TEXT NOT NULL REFERENCES modules(hash),
    verdict     TEXT,                   -- NULL while judging; then AC|WA|TLE|MLE|RE|OOF
    detail      TEXT,                   -- JSON array of per-case results
    created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS apps (
    name         TEXT PRIMARY KEY,      -- [a-z0-9-]+, validated
    backend_hash TEXT REFERENCES modules(hash),   -- nullable: static-only apps allowed
    created_at   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS assets (
    app          TEXT NOT NULL REFERENCES apps(name),
    path         TEXT NOT NULL,         -- 'index.html', 'pkg/app_bg.wasm', ...
    content_type TEXT NOT NULL,
    bytes        BLOB NOT NULL,
    PRIMARY KEY (app, path)
);
```

## E4. Dependency additions (`engine/Cargo.toml`)

```toml
zip = "2"              # phase 6 asset bundle upload
mime_guess = "2"       # phase 6 content types
```

Toolchain additions (phase 6 only): `cargo install trunk` and the existing
`wasm32-unknown-unknown` target (already installed for guests) — Trunk uses it for the
Leptos frontend too.

---

# PHASE 4 — Functions (stateless serverless + workflow bridge)

## Task 4.1 — Runtime config, epoch ticker, workflow fuel watchdog

Rebuild the wasmtime `Config` per §E1 (`consume_fuel(true)` + `epoch_interruption(true)`,
still ONE Engine and ONE component cache), spawn the 100ms ticker thread, add
`--wf-fuel-limit` (default 10^13), and apply the workflow limit profile in the base
runner per §E1's retrofit paragraph. Re-run `accept_phase1.sh` and `accept_phase3.sh`
after this task — enabling fuel and epochs engine-wide must not disturb the base engine
(if a base test now fails with a fuel or epoch trap, a Store is missing its profile
setup; every Store must get one of the two profiles, no exceptions).

## Task 4.2 — `handler` bindings + platform-api host impl

`FnCtx` (store data for handler instances): `db: rusqlite::Connection` (via
`open_conn`), `shared: Arc<EngineShared>`, `mem_limiter: MemLimiter` (Task 5.1 —
define the struct now, enforcement lands in phase 5).

platform-api impls: `log` → tracing with a `fn:` prefix; `now_ms`/`random_u64` →
direct, no journal (functions are not durable — see §E1); `start_workflow(hash, input)`
→ validate hash exists in `modules`, insert workflows row (status `running` via
`set_status`), then `runner::spawn`. **This amends the base-spec rule: the sanctioned
`runner::spawn` callsites are now FOUR** — creation endpoint, recovery scan, upgrade
step 5, and this host function. `get_workflow(id)` → SELECT row → serialize the same
JSON shape as `GET /api/workflows/:id` (share the serializer, don't duplicate it).

## Task 4.3 — Routes + dispatcher

API: `POST /api/routes {"prefix":"/fn/echo","module_hash":"...", (optional limits)}`
→ 201; `GET /api/routes` → list; `DELETE /api/routes/{prefix}` → 204. Validate prefix:
starts with `/fn/`, no trailing slash, no `..`.

Dispatcher: an axum fallback handler for `/fn/*`:

1. Longest-prefix match against `routes` (load all prefixes; N is small; no cleverness).
   No match → 404 JSON.
2. Read body with a 10 MiB cap (`413` beyond). Build the WIT `http-request`: `path` =
   full path minus prefix (ensure leading `/`, empty → `/`), raw query, headers as
   string pairs (skip non-UTF-8 header values), body bytes.
3. `tokio::task::spawn_blocking`: fresh `open_conn`, fresh `Store<FnCtx>` with the
   function profile — `store.set_fuel(route.fuel_limit)`;
   `store.set_epoch_deadline(ceil(time_limit_ms / 100))`; `epoch_deadline_trap()` —
   instantiate from the component cache; `call_handle(req)`; measure wall time.
4. Outcome classification (this exact order — it is reused verbatim by the phase-5
   judge, so put it in one function `classify(result, &mem_limiter) -> Outcome`):
   limiter `denied` flag set → `mle`; else `Err` downcasting to
   `wasmtime::Trap::OutOfFuel` → `oof`; else `Err` downcasting to
   `wasmtime::Trap::Interrupt` → `tle`; else `Err` → `trap`; else `ok`.
   FALLBACK: if the Trap variant names differ in your wasmtime, `cargo doc` the `Trap`
   enum; epoch expiry is the "interrupt"-flavored variant, fuel exhaustion the
   "out of fuel" one.
5. `ok` → relay guest status/headers/body (drop `content-length`, axum sets it).
   Any other outcome → HTTP 500 with JSON `{"outcome":"tle"|...}`.
6. ALWAYS insert an `invocations` ledger row: kind `function`, ref = prefix,
   fuel_used = `route.fuel_limit - store.get_fuel().unwrap_or(0)`, peak_mem from the
   limiter, duration_ms, outcome. The ledger row is written even on failure — metering
   that only counts successes is fiction.

## Task 4.4 — Guests: `guests/echo-fn`, `guests/starter-fn`

`echo-fn` (world `handler`): returns 200, header `x-echo-method`, body =
`{"method":M,"path":P,"query":Q,"body_len":N}`.

`starter-fn` (world `handler`): routes on `req.path`: `POST /start` → parse body JSON
`{"module_hash": "...", "input": {...}}` → `start-workflow` → 202 with
`{"workflow_id": id}`; `GET /status` with query `id=...` → `get-workflow` → relay the
JSON, 200. Anything else 404. (This same component is reused as the phase-6 app
backend — write it once, cleanly.)

## Task 4.5 — Routes UI

`/routes` page (htmx, same tokens as base): table (prefix, module, limits, invocation
count from ledger), create form ("Bind route"), delete buttons ("Remove route"). Add
"Routes", "Playground" (phase 5), "Apps" (phase 6), "Usage" (phase 5) to the nav in
`base.html` now so later phases only fill pages in.

## Task 4.6 — Acceptance (`scripts/accept_phase4.sh`)

Fresh DB; engine with default flags. Steps:

1. Build engine + echo-fn + starter-fn + counter (base phase 3, default features).
2. Upload all three; bind `/fn/echo` → echo-fn, `/fn/jobs` → starter-fn.
3. `curl -s -X POST localhost:8080/fn/echo/deep/path?x=1 -d 'ping'` → assert HTTP 200,
   body contains `"path":"/deep/path"` and `"body_len":4`.
4. Assert ledger: `SELECT outcome, fuel_used FROM invocations WHERE ref='/fn/echo'`
   → outcome `ok`, fuel_used > 0.
5. `curl -X POST /fn/jobs/start -d '{"module_hash":"<counter>","input":{"target":3}}'`
   → extract workflow_id → poll `GET /fn/jobs/status?id=...` (through the FUNCTION, not
   the base API — proving the bridge) until the relayed JSON shows `completed` (≤60s)
   and output contains `"count":3`.
6. `curl /fn/nope` → 404.

Definition of done: `PHASE 4 PASS` twice from clean, AND base acceptance 1 & 3 still
pass after the refactor.

---

# PHASE 5 — Sandbox limits, metering, playground

## Task 5.1 — Memory limiter

```rust
pub struct MemLimiter { pub limit: usize, pub peak: usize, pub denied: bool }
impl wasmtime::ResourceLimiter for MemLimiter {
    fn memory_growing(&mut self, _cur: usize, desired: usize, _max: Option<usize>)
        -> wasmtime::Result<bool> {
        self.peak = self.peak.max(desired);
        if desired > self.limit { self.denied = true; Ok(false) } else { Ok(true) }
    }
    fn table_growing(&mut self, _c: usize, d: usize, _m: Option<usize>)
        -> wasmtime::Result<bool> { Ok(d <= 1_000_000) }
}
```

Wire with `store.limiter(|ctx| &mut ctx.mem_limiter)` on ALL function and solver stores
(functions and solvers). Do NOT apply to workflow stores in this phase (their memory
story is bound up with snapshots; out of scope).

## Task 5.2 — Judge

Judging constants (top of `judge.rs`): per-case fuel 1_000_000_000, mem 256 MiB, time
2000ms. Flow for `POST /api/submissions {"problem": slug, "module_hash": hash}`:

1. Insert submission (verdict NULL), return 202 with id, then judge on
   `tokio::task::spawn_blocking` (NOT inline in the handler).
2. For each case in `idx` order: fresh Store with the function profile (world `solver` — note it
   has ZERO imports; the linker needs nothing), set fuel/deadline/limiter, call
   `solve(input)`. Classify with the SAME `classify()` from Task 4.3, then map:
   `mle→MLE`, `oof→OOF`, `tle→TLE`, `trap→RE`, guest `Err(_)`→RE, guest `Ok(out)` →
   `AC` if `out.trim() == expected.trim()` else `WA`.
3. Ledger row per case (kind `solve`, ref = submission id). Stop at first non-AC.
4. Final verdict = first non-AC or AC; `detail` = JSON array
   `[{"idx","verdict","fuel","peak_mem","ms"}, ...]`; single UPDATE at the end.

## Task 5.3 — Playground API + UI

API: `POST /api/problems` `{"slug","title","statement","cases":[{"input","expected"},…]}`
(idempotent upsert; this is the operator seeding endpoint). UI: `/playground` lists
problems; `/playground/:slug` shows statement (`<pre>`), a submit form that uploads a
`.wasm` file (store via the existing modules flow, then create the submission), and an
htmx-polled submissions table: verdict badge (AC green, WA amber, TLE blue, MLE purple,
RE/OOF red — reuse the status-badge CSS pattern) plus fuel and peak-mem columns per
submission. The metering columns are the point — an untrusted-code platform that shows
you exactly what each submission cost.

## Task 5.4 — Usage page

`/usage`: htmx-polled table of the last 100 `invocations` (time, kind, ref, module
short-hash, outcome, fuel, peak mem, ms) + a totals-by-module summary row set
(`SELECT module_hash, COUNT(*), SUM(fuel_used) ... GROUP BY module_hash`).

## Task 5.5 — Sample solvers + the runaway workflow guest

- `guests/sum-solver` (world `solver`): input = first line N, second line N ints,
  space-separated; output = their sum as a string. Feature flag `wrong`: adds 1
  (produces WA — same single-crate two-artifact pattern as counter v1/v2).
- `guests/loop-solver`: `solve` enters `loop {}` (TLE fodder). Add
  `#[allow(unreachable_code)]` as needed.
- `guests/hog-solver`: repeatedly `Vec::push` 1 MiB chunks forever (MLE fodder).
- `guests/spin-workflow` (world `workflow`): `run` enters `loop {}` — for the runaway
  retrofit test. Implement `resume` as the stub.

## Task 5.6 — Acceptance (`scripts/accept_phase5.sh`)

Fresh DB. Engine launched with `--wf-fuel-limit 10000000` (a starvation budget, to
test the workflow watchdog).

1. Seed problem `sum` with cases `("2\n1 2","3")` and `("3\n10 20 30","60")`.
2. Build + upload all four phase-5 guests plus sum-solver-wrong.
3. Submit sum-solver → poll verdict → `AC`; assert `detail` has 2 entries, both AC,
   each with fuel > 0; assert 2 ledger rows kind=`solve` for this submission.
4. Submit sum-solver-wrong → `WA`. Submit loop-solver → `TLE` (must resolve in ~2–4s,
   not hang — assert the poll completes within 30s). Submit hog-solver → `MLE`.
5. Function-side limit: rebind `/fn/echo` with `fuel_limit=1000` (a starvation budget);
   curl it → HTTP 500 with `"outcome":"oof"`; ledger row `oof`.
6. Workflow watchdog: start spin-workflow → poll until status `failed` (≤30s); assert
   output contains `compute budget` (the §E1 runaway message); assert the engine is
   still healthy (`curl /` → 200).

Definition of done: `PHASE 5 PASS` twice from clean; base + phase 4 scripts still green
(run phase-4's with default deadline).

---

# PHASE 6 — Hosted full-stack apps (Rust→WASM frontend + backend function)

## Task 6.1 — Apps + assets

API:
- `POST /api/apps {"name":"hello","backend_hash":"<optional>"}` — name must match
  `[a-z0-9-]{1,32}`; 201.
- `POST /api/apps/:name/assets` — body = a zip (raw bytes; `DefaultBodyLimit` 64 MiB).
  Iterate entries with the `zip` crate: skip directories; REJECT any entry whose path
  contains `..` or starts with `/` (400 — zip-slip is a real attack, not paranoia);
  normalize to forward slashes; content type via `mime_guess` with fallback
  `application/octet-stream`, but force `.wasm → application/wasm` and
  `.js → text/javascript` explicitly (mime_guess is fine, trust but verify these two —
  the browser refuses to instantiate WASM served with the wrong type in some paths).
  Upsert every entry into `assets`. Response: `{"stored": N}`.

Serving, as an axum fallback on `/apps/:name/*rest`:
1. `rest` empty or `/` → serve `index.html`.
2. Exact match in `assets` → serve bytes with stored content type and
   `cache-control: no-store` (this is a dev platform; no cache-invalidation puzzles).
3. `rest` starts with `api/` → if `backend_hash` is set, invoke it EXACTLY like a
   phase-4 route (reuse the dispatcher core as a function taking
   `(module_hash, limits, request)` — refactor Task 4.3 so route dispatch and app
   dispatch share one code path; default limits constants). The `path` handed to the
   guest is `rest` minus the `api` prefix (`/apps/hello/api/start` → guest sees
   `/start`). No backend → 404.
4. No match and `rest` has no file extension → serve `index.html` (SPA fallback for
   client-side routing). Otherwise 404.

## Task 6.2 — The `hello` app (Leptos CSR + Trunk), in `apps/hello/`

This is a NORMAL Rust crate (not cargo-component; Trunk drives the build). Files:

`apps/hello/Cargo.toml`:
```toml
[package]
name = "hello"; version = "0.1.0"; edition = "2021"
[dependencies]
leptos = { version = "0.7", features = ["csr"] }
gloo-net = "0.6"
wasm-bindgen-futures = "0.4"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
# FALLBACK ladder if leptos 0.7 APIs have shifted: try the latest leptos with "csr";
# if the view!/signal APIs still fight the builder after one honest attempt, replace
# the frontend with a plain wasm-bindgen + web-sys click handler. The acceptance
# below only requires: Rust-compiled WASM served by the platform + the API roundtrip.
```

`apps/hello/index.html`:
```html
<!DOCTYPE html><html><head><meta charset="utf-8">
<title>hello — keel app</title>
<link data-trunk rel="rust" data-wasm-opt="0"/>
</head><body></body></html>
```

`apps/hello/src/main.rs` — a counter-of-jobs UI: one button "Start job" that
`POST ./api/start` with body `{"module_hash": MODULE_HASH, "input": {"target": 3}}`
(MODULE_HASH is a `const` the acceptance script injects via `sed` before building —
crude, honest, effective), stores the returned `workflow_id` in a signal, then polls
`GET ./api/status?id=...` every second via `spawn_local` + gloo-net until the JSON
contains `completed`, rendering live status text and finally the output. Use RELATIVE
URLs (`./api/...`) so the app is mount-point agnostic. ~60 lines; keep it boring.

Build: `cd apps/hello && trunk build --release` → `dist/` containing `index.html`,
`hello-*.js`, `hello-*_bg.wasm`. Bundle: `(cd dist && zip -r ../hello.zip .)`.

## Task 6.3 — Apps UI

`/apps` page: list (name, backend module, asset count, "Open app" link to
`/apps/:name/`), create form, zip upload form per app.

## Task 6.4 — Acceptance (`scripts/accept_phase6.sh`)

Fresh DB.

1. Build engine, counter (base, default), starter-fn; upload both; `sed` the counter
   hash into `apps/hello/src/main.rs`; `trunk build`; zip.
2. Create app `hello` with `backend_hash` = starter-fn's hash; upload the zip; assert
   `stored >= 3`.
3. `curl -s localhost:8080/apps/hello/` → 200, contains `<script` and `.wasm`.
4. `WASM_PATH=$(sqlite3 $DB "SELECT path FROM assets WHERE app='hello' AND path LIKE
   '%.wasm'")`; `curl -sI localhost:8080/apps/hello/$WASM_PATH` → 200 AND
   `content-type: application/wasm`; body size > 100000 bytes.
5. The vertical slice, via the same endpoints the frontend button calls:
   `curl -X POST /apps/hello/api/start -d '{"module_hash":"<counter>","input":{"target":3}}'`
   → workflow_id; poll `GET /apps/hello/api/status?id=...` until `completed` and
   `"count":3` (≤60s). This proves browser-served-assets + function + durable workflow
   in one binary; the literal button click is verified by a human once (the script
   prints the URL and says so — headless-browser testing is deliberately out of scope).
6. Zip-slip: upload a zip containing `../evil.txt` → 400, and
   `SELECT COUNT(*) FROM assets WHERE path LIKE '%evil%'` == 0.

Definition of done: `PHASE 6 PASS` twice from clean; phases 1–5 scripts all still green.
This is the platform's flagship demo: open `/apps/hello/`, click "Start job", watch a
durable workflow tick to completion inside a page whose UI is itself Rust compiled to
WASM, all served by one process and one SQLite file.

---

# Troubleshooting additions (extends the base table)

| Symptom | Likely cause | Fix |
|---|---|---|
| Workflow fails OutOfFuel during legitimate replay | wf fuel budget too small for the oplog length | raise `--wf-fuel-limit`; base phase-3 checkpoints exist precisely to bound replay cost |
| Function/solver never times out | epoch ticker thread not running, or `epoch_deadline_trap()` not set on that store | ticker starts in `serve` before axum; deadline + trap set per store |
| `Trap` downcast never matches | comparing against the wrong error layer | downcast the returned error to `wasmtime::Trap` first, then match variants; check `cargo doc` for exact variant names |
| MLE reported as RE | classifying by trap before checking the limiter | `classify()` checks the `denied` flag FIRST (Task 4.3 order is normative) |
| Browser won't run the app's WASM | wrong content type on the `.wasm` asset | forced `application/wasm` in Task 6.1; verify with `curl -I` |
| App fetches 404 under `/apps/name/` | frontend used absolute `/api/...` URLs | relative `./api/...` only (Task 6.2) |
| `bindgen!` name collisions across three worlds | shared WIT records generated twice | separate Rust modules per bindgen; then `with:` remapping per wasmtime docs |

# Stretch directions (explicitly NOT in any acceptance gate)

`wasi:http/proxy` compatibility mode via the `wasmtime-wasi-http` crate — a fourth
world the dispatcher can drive, letting unmodified ecosystem components (including
Spin-style apps) run on Keel; a `wasi:keyvalue` host interface backed by a `kv` table
for both functions and (journaled!) workflows; per-route rate limiting off the ledger;
and the base spec's deferred physical memory snapshots. Each of these is a spec
amendment first, code second — same discipline as everything above.

# Execution notes for the builder

Phase order is 4 → 5 → 6, strictly. Phase 4's refactors (runtime limit profiles, dispatcher core)
are load-bearing for 5 and 6 — do them as specified, not minimally. Every phase's
definition-of-done includes re-running ALL earlier acceptance scripts; the platform is
one binary, and a regression in the workflow engine caused by the functions refactor is
a phase-4 failure even though phase 1's script catches it. When in doubt between
"clever" and "boring", choose boring: this spec's job is to be buildable, and the
platform's job is to make WASM's guarantees visible, not to show off Rust.
