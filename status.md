# Keel — build status & hand-off notes

This file is the continuation handbook for whoever (or whatever model) builds next.
The source of truth for WHAT to build is [SPEC.md](SPEC.md) (a copy of
`../durable-engine-build-spec.md`, rev 1.1). This file records what exists, what was
verified, every deviation from the spec and why, and exactly where Phase 2 starts.

**Before writing any code: re-read SPEC.md §0 ("Rules for the builder") — the spec
itself demands re-reading it at each phase.**

---

## TL;DR — where things stand

- **Phase 1: COMPLETE and VERIFIED.** `scripts/accept_phase1.sh` printed
  `PHASE 1 PASS` twice in a row on 2026-07-15 (definition of done met).
- **Phase 2: NOT STARTED.** Begin at SPEC.md Task 2.1. Pointers + landmines below.
- **Phase 3: NOT STARTED.**

Build/run cheatsheet (run from this directory, `keel/`):

```bash
cargo build --release -p keel-engine                # engine → target/release/keel
(cd guests/demo && cargo component build --release --target wasm32-unknown-unknown)
./scripts/accept_phase1.sh                          # must print PHASE 1 PASS
```

---

## Environment (verified on this machine, 2026-07-15)

| Thing | State |
|---|---|
| OS | macOS (Darwin 25.5.0) — spec's `apt-get install sqlite3` is Linux-only; macOS ships sqlite3 3.51, already fine |
| rustc / cargo | 1.96.1 |
| wasm32-unknown-unknown target | installed (`rustup target add wasm32-unknown-unknown`) |
| cargo-component | 0.21.1 (installed via `cargo install cargo-component --locked`); lives in `~/.cargo/bin` — make sure that's on PATH |
| sqlite3 CLI | 3.51.0 (used by acceptance scripts only) |
| python3 | 3.13 (used by acceptance scripts only) |

## Version resolutions (SPEC.md §0 rule 5)

| Dependency | Spec said | Resolved to | Note |
|---|---|---|---|
| wasmtime | "43" | **43.0.2** | pin resolved as-is; API differs from spec's examples in two places — deviations 2 and 3 below |
| rusqlite | "0.32" | 0.32.1 | as spec'd |
| axum | "0.8" | 0.8.9 | route syntax gotcha — deviation 1 |
| ureq | "2" | 2.12.1 | as spec'd (do NOT bump to ureq 3; API differs) |
| wit-bindgen-rt (guest) | n/a | 0.44.0 | must match what cargo-component 0.21.1 generates; taken from its own template |

`Cargo.lock` is committed — trust it over this table.

---

## Deviations from the spec (all deliberate, each marked in code comments too)

1. **axum 0.8 path params are `{id}`, not `:id`.** The spec's route table (Task 1.5)
   uses 0.7-era `:id`, which *panics at router build time* in axum 0.8. Routes in
   `main.rs` use `/api/workflows/{id}`. External URLs are unchanged.

2. **`bindgen!` option renamed.** Spec-era `trappable_imports: true` became
   `imports: { default: trappable },` in wasmtime 43 (`engine/src/host.rs`). Effect is
   identical: host fns return `wasmtime::Result<T>` so a journal/db failure traps the
   guest instead of panicking.

3. **wasmtime 43 has its own `Error` type (no longer an alias of `anyhow::Error`).**
   The engine stays anyhow-internal (journal.rs is spec-verbatim). Conversions:
   - `wasmtime::Error` → `anyhow::Error`: automatic via `?` (used all over runner.rs).
   - `anyhow::Error` → `wasmtime::Error`: NOT automatic; the single `trap()` helper in
     host.rs wraps `wasmtime::Error::from_anyhow`. Every `journaled(...)` call site in
     host.rs ends `.map_err(trap)?`. Keep that pattern for await-event/checkpoint later.
   - anyhow's `.context()` doesn't attach to `Result<_, wasmtime::Error>` — see the
     `map_err(anyhow::Error::from).context(...)` dance in runner.rs.

4. **`random-u64` values are masked to 63 bits** (`& i64::MAX`, host.rs). Reason: the
   acceptance script compares the journaled value against guest output *textually* via
   sqlite's `json_extract`, which loses precision above i64::MAX (returns a float).
   A full-range u64 would make acceptance flaky ~50% of the time. ~62 effective random
   bits from a uuid-v4 (no `rand` dependency). Still returns u64 to the guest.

5. **Workspace `exclude = ["guests"]`** (root Cargo.toml). The spec says to keep guests
   out of `[members]`; `exclude` is additionally required or cargo errors on any build
   under `guests/` ("package is not a member of the workspace").

6. **Spec typo in Task 1.2**: `journaled("sleep-ms", &Req { ms }, |_| {...})` — the
   closure takes no arguments per the §6 signature (`FnOnce()`); implemented as `||`.

7. **Workflow rows are INSERTed with status `'running'`** (db.rs::create_workflow),
   BEFORE `runner::spawn`. The spec doesn't pin the initial status; 'running' makes
   creation crash-safe (die between INSERT and spawn → the recovery scan picks it up).

8. **`db.rs` owns every SQL statement — except journal.rs.** The spec demands both
   "keep every SQL statement in db.rs" (§ execution notes) and "implement journal.rs
   exactly" (§6, which contains SQL). §6 wins for journal.rs; everything else goes
   through db.rs helpers.

Non-deviations worth knowing: `component-model` is a default wasmtime-43 feature (the
spec's FALLBACK note applies but the explicit feature is harmless); `bindgen!` found
the wit dir at `path: "../wit"` without needing the engine/wit copy fallback.

---

## What exists (Phase 1 file map)

```
keel/
├── Cargo.toml               workspace = ["engine"], exclude guests (deviation 5)
├── SPEC.md                  the build spec — THE source of truth
├── status.md                this file
├── README.md                includes the REQUIRED verbatim runaway-guest warning (§0 non-goals)
├── wit/workflow.wit         0.1.0 contract — §4 verbatim. Phase 2 bumps to 0.2.0
├── engine/src/
│   ├── main.rs              Task 1.1 CLI (`keel serve --db --listen`) + Task 1.4 recovery scan + router
│   ├── db.rs                §5 schema + open_conn() + set_status() + all SQL helpers
│   ├── journal.rs           §6 journaled() core — SPEC-VERBATIM, the heart of the engine
│   ├── host.rs              Task 1.2 bindgen! + host-api impl (http-get/sleep-ms/now-ms/random-u64/log)
│   ├── runner.rs            Task 1.3 EngineShared + one-thread-per-workflow + status transitions
│   └── api.rs               Task 1.5 JSON API (4 endpoints)
├── guests/demo/             Task 1.6 acceptance guest (src/bindings.rs is GENERATED — don't edit)
└── scripts/accept_phase1.sh Task 1.7 — spec-verbatim acceptance
```

Read the header comment of each file first — they carry the invariants and mark every
PHASE 2 / PHASE 3 surgery point with the task number.

## Invariants you must not break (condensed from SPEC.md §0)

1. Journal row COMMITS BEFORE the result returns to the guest. Never reorder.
2. NO replay mode. Recovery = run from the beginning; journaled() returns recorded
   rows. `if replaying` anywhere = architecture error.
3. Guests have zero ambient capabilities; only host-api. Built for wasm32-unknown-unknown.
4. Every SQLite connection comes from `db::open_conn()` (per-connection pragmas!),
   one connection per thread.
5. Every workflows write goes through `db::set_status()` (updated_at is NOT NULL);
   creation via `db::create_workflow()`.
6. `runner::spawn` call-sites: api.rs creation, main.rs recovery scan, phase-3 upgrade
   handler step 5. Never anywhere else.
7. Acceptance scripts are the definition of done. Fix the engine, never the script.
8. Commit per numbered task, message = "task number + name".

---

## Verification record

- `cargo build` (debug + release): clean, **0 warnings**.
- `guests/demo`: builds with cargo-component 0.21.1 → 27KB component.
- `scripts/accept_phase1.sh`: **PHASE 1 PASS, twice in a row** (2026-07-15).
  Evidence from the run, in case you need to compare against a regression:
  - engine.log shows `recovering workflow <id>` after the kill -9 restart, and the
    replayed "first fetch" guest log line lands ~100µs after workflow start (journal
    read, no network) vs the live run's ~120ms — replay path confirmed.
  - journal = exactly 4 rows: `random-u64(0), http-get(1), sleep-ms(2), http-get(3)`;
    exactly 2 http-get rows total (pre-crash call NOT re-executed).
  - workflows.output stamp == journaled random-u64 value (deterministic replay).
  - Duplicate guest log lines after restart are EXPECTED (log is not journaled, §4.1).

Git history note: phase 1 was built in a single pass and committed per-task
afterwards; individual historical commits group files by task but were not each
built in isolation. HEAD is the verified tree.

---

## Phase 2 — where to start and what will bite

Work Tasks 2.1 → 2.10 in order (SPEC.md). File-level pointers:

- **2.1 WIT 0.2.0**: edit `wit/workflow.wit` (bump package version, add `await-event`
  to host-api). `cargo build` regenerates engine bindings; add the new method to the
  `Host` impl in host.rs (return type `wasmtime::Result<String>`, `.map_err(trap)?`
  pattern). Adding an import is non-breaking for existing guests.
- **2.2 schema**: APPEND `timers` + `events` tables to `MIGRATION` in db.rs. Never
  ALTER. Note `events.delivered_seq` exists specifically so phase-3 upgrades can
  un-deliver events (SPEC.md explains).
- **2.3 notifier.rs**: new file + `mod notifier;` in main.rs. get-or-insert semantics
  on BOTH wait and notify (a notify before the first wait must not be lost). The
  Notifier is a latency optimization ONLY — every park loop still polls the DB at 1s.
  Add it + abort-set to `EngineShared` and a handle into `Ctx` (host.rs).
- **2.4 durable sleep**: REPLACES host.rs sleep_ms. Hand-rolled journal check + timers
  row + park loop — do NOT force it through journaled() (the spec shows the exact
  algorithm). Status flips to 'sleeping' — remember set_status, and notifier abort
  checks (they're phase-3 hooks but the spec wants them in the loop now).
- **2.5 events**: await_event host fn + POST /api/workflows/{id}/events endpoint in
  api.rs (+ notify). The deliver+journal single transaction is MANDATORY — use
  `rusqlite::Connection::transaction()` on the JournalCtx's connection.
- **2.6 http retries**: inside do_http_get only (live path). 3 attempts, 500ms/1s/2s,
  retry transport + ≥500 only. One journal row regardless.
- **2.7 --max-running**: default 256, `Arc<(Mutex<u32>, Condvar)>` permit counter in
  EngineShared, acquired at thread start. Print the starvation warning at startup
  when >80% held (spec text).
- **2.8 htmx UI**: new ui.rs + templates/ + assets/. Vendor htmx locally (COMMIT it),
  build.rs that panics if missing. NO askama_axum — render to String, wrap in
  `axum::response::Html`. askama 0.12 is already in Cargo.toml (unused so far).
  Exact colors/copy rules are in the spec — follow them literally.
  NOTE: axum route syntax `{id}` (deviation 1) applies to all new routes.
- **2.9 approval guest**: copy guests/demo, adjust (await-event("approve") →
  sleep 60s → output). Same Cargo.toml shape (wit-bindgen-rt 0.44).
- **2.10 acceptance**: write scripts/accept_phase2.sh in the same style; the spec
  lists the steps/assertions. Reminder: after restart, poll up to 15s for
  `waiting_event` (recovery passes through 'running' — immediate assert = flake).

Phase 3 afterwards: WIT 0.3.0 is a BREAKING guest change (adds `resume` export);
all guests get a stub `resume` then; fresh DB for acceptance.

## Debugging crib

- Engine logs: stdout (acceptance scripts redirect to `engine.log`).
- Inspect a run: `sqlite3 accept1.db 'SELECT seq,kind,request,response FROM journal ORDER BY seq'`
  and `SELECT id,status,output FROM workflows`.
- Consult the Troubleshooting table at the bottom of SPEC.md BEFORE improvising —
  most "weird" failures are listed there with fixes.
- Kill a stray engine: `pkill -f 'keel serve'` (acceptance scripts leave none on
  success, but a failed run can).
