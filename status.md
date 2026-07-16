# Keel — build status & hand-off notes

This file is the continuation handbook for whoever (or whatever model) works here
next. The source of truth for WHAT was built is [SPEC.md](SPEC.md) (a copy of
`../durable-engine-build-spec.md`, rev 1.1). This file records what exists, what was
verified, and every deviation from the spec and why.

**Before writing any code: re-read SPEC.md §0 ("Rules for the builder").**

---

## TL;DR — where things stand

- **ALL THREE PHASES: COMPLETE and VERIFIED. The spec is fully built.**
- Phase 1 (journal + replay + kill -9 recovery): `PHASE 1 PASS` ×2 on 2026-07-15.
- Phase 2 (durable timers, events, UI, cap): `PHASE 2 PASS` ×2 on 2026-07-16.
- Phase 3 (checkpoints, pruning, live upgrade): `PHASE 3 PASS` ×2 on 2026-07-16.
- All three suites re-ran green at HEAD (fresh DBs) after the last commit — the
  phase-3 WIT/runner changes did not regress earlier phases.

Build/run cheatsheet (run from this directory, `keel/`):

```bash
cargo build --release -p keel-engine                # engine → target/release/keel
(cd guests/demo && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/approval && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/counter && cargo component build --release --target wasm32-unknown-unknown)  # +--features v2
./scripts/accept_phase1.sh                          # must print PHASE 1 PASS
./scripts/accept_phase2.sh                          # must print PHASE 2 PASS
./scripts/accept_phase3.sh                          # must print PHASE 3 PASS
```

UI: `http://127.0.0.1:8080/` (dashboard), `/modules` (upload + start), each
workflow at `/workflows/<id>` with a 2s-polling detail, "Send event" form, and —
when parked + checkpointed — an "Upgrade module" control.

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
| askama | "0.12" | 0.12.1 | phase 2 UI; NO askama_axum on purpose (render to String → axum Html) |
| htmx (vendored) | "@2" | 2.0.10 | committed at `engine/assets/htmx.min.js`; build.rs fails the build if it goes missing |

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
   through db.rs helpers. Phase 2 kept this: the park-loop host calls use db.rs
   helpers (`get_journal_row`/`insert_journal_row`/timers/events) instead of inline
   SQL, and the single-transaction event delivery lives in
   `db::deliver_event_and_journal`.

Phase 2 additions (2026-07-16):

9. **Park-loop status writes happen in host.rs, not runner.rs.** Phase 1's note
   "status transitions live in runner.rs only" was superseded by the spec's own
   Task 2.4/2.5 pseudocode (`UPDATE status='sleeping'` inside the host call).
   Terminal transitions (completed/failed) are still runner.rs-only; every write
   still goes through `db::set_status`.
10. **`AbortForUpgrade` is already a real error type** (`host.rs`), not a bail
    string — Task 3.5's downcast (`root_cause().downcast_ref::<AbortForUpgrade>()`)
    will work without reworking the park loops. Nothing sets the abort flag until
    phase 3; the loops' `is_aborted` checks are live but dormant.
11. **The three write endpoints accept a second body shape** (Task 2.8 requires
    it for the UI forms): `/api/modules` takes raw bytes OR multipart
    (`file` + `name` fields); `/api/workflows` and `/api/workflows/{id}/events`
    take JSON OR urlencoded forms (JSON carried as text in `input`/`payload`
    fields, validated server-side). Implementation: handlers take `Request` and
    branch on content-type via `FromRequest` — the JSON paths are byte-identical
    to phase 1. hx-post sends urlencoded, hence the form shape on events.
12. **HTTP retry backoff schedule `[500ms, 1s, 2s]` with 3 attempts** means only
    the first two gaps are reachable; the spec lists all three values, so the 2s
    slot is coded (and documented) as the next step if attempts are ever raised.

Phase 3 additions (2026-07-16):

13. **checkpoint's exec closure opens a scoped SECOND Connection** (host.rs). The
    spec routes checkpoint through `journaled()`, but exec is `FnOnce()` and
    cannot borrow `self.j.db` while journaled holds `&mut self`. Ctx carries
    db_path for this. Safe because the snapshot+prune txn and the wrapper's
    row-C INSERT are two separate transactions BY DESIGN (spec: "the wrapper then
    inserts journal row C as usual"); a crash between them leaves the snapshot at
    C with row C missing — tolerated, since resume starts at C+1 and never reads
    row C. This is the one place the one-connection-per-thread rule bends.
14. **Bounded join by polling `JoinHandle::is_finished`** (api.rs upgrade step 3)
    instead of the spec's `spawn_blocking(join)` + timeout sketch. Reason: on
    timeout, spawn_blocking has consumed the handle, so a retry would find the
    registry empty, conclude "thread already exited", and step 5 would spawn a
    SECOND live worker on the same journal (nondeterminism). Polling keeps the
    handle owned; on timeout it goes BACK into the registry (`put_thread`). The
    final `join()` only runs once `is_finished()` is true, so nothing blocks the
    async handler.
15. **The upgrade handler re-validates after the join** (status still parked,
    snapshot re-read for a FRESH C) and **clears the abort flag unconditionally**
    after the join/None resolution. Guards two races the spec's steps leave open:
    a worker that wakes and advances (or completes) between validation and the
    abort landing must not be resurrected from a stale C; and on the
    thread-already-exited path a set-but-never-observed abort flag would
    instantly kill the respawned worker at its first park.

Non-deviations worth knowing: `component-model` is a default wasmtime-43 feature (the
spec's FALLBACK note applies but the explicit feature is harmless); `bindgen!` found
the wit dir at `path: "../wit"` without needing the engine/wit copy fallback.

One WIT-versioning caveat for later phases: bumping the package to 0.2.0 required
REBUILDING both guests (bindings.rs is generated from the wit). "Adding an import is
non-breaking" is a source-level promise — an old `.wasm` BLOB compiled against
`host-api@0.1.0` will not instantiate against a linker that only exports 0.2.0
(0.x minors are semver-incompatible). Fresh-DB acceptance never sees this, but a
long-lived deployment upgrading the engine under stored modules would. Phase 3 bumps
to 0.3.0 and is breaking by design (adds a `resume` export) — all guests get a stub
`resume` then, per Task 3.1.

---

## What exists (file map, all phases)

```
keel/
├── Cargo.toml               workspace = ["engine"], exclude guests (deviation 5)
├── SPEC.md                  the build spec — THE source of truth
├── status.md                this file
├── README.md                includes the REQUIRED verbatim runaway-guest warning (§0 non-goals)
├── wit/workflow.wit         0.3.0 contract: http-get/sleep-ms/now-ms/random-u64/await-event/
│                            checkpoint/log imports; run + resume exports
├── engine/
│   ├── build.rs             Task 2.8 — fails the build if assets/htmx.min.js is missing
│   ├── assets/              htmx.min.js (2.0.10, vendored+committed) + style.css (exact spec palette)
│   ├── templates/           askama: dashboard, _workflows_table, workflow, _workflow_detail, modules
│   └── src/
│       ├── main.rs          CLI (serve --db --listen --max-running) + recovery scan + router
│       ├── db.rs            full schema (5 tables) + open_conn() + set_status() + ALL SQL helpers (dev. 8)
│       ├── journal.rs       §6 journaled() core — SPEC-VERBATIM, the heart of the engine
│       ├── host.rs          host-api impl; park loops (2.4/2.5); checkpoint (3.3); AbortForUpgrade
│       ├── notifier.rs      condvar wake-ups + abort set (latency only, never correctness)
│       ├── runner.rs        EngineShared + thread-per-workflow + Permit cap + thread registry +
│       │                    snapshot-aware start (resume) + abort-sentinel result match
│       ├── api.rs           JSON API, 6 endpoints incl. upgrade; several also accept form/multipart
│       └── ui.rs            askama-render-to-Html handlers + embedded assets + upgrade control
├── guests/demo/             Task 1.6 acceptance guest (src/bindings.rs is GENERATED — don't edit)
├── guests/approval/         Task 2.9 acceptance guest: await-event("approve") → sleep 60s → output
├── guests/counter/          Task 3.7 acceptance guest: v1/v2 via feature flag; ticks + checkpoints
└── scripts/
    ├── accept_phase1.sh     Task 1.7 — spec-verbatim acceptance
    ├── accept_phase2.sh     Task 2.10 — kill -9 at both park points; W1==W2; UI smoke
    └── accept_phase3.sh     Task 3.8 — pruning; resume recovery; v1→v2 live upgrade; 409 negative
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

- `cargo build` (debug + release): clean, **0 warnings** (the two
  `#[allow(dead_code)]` in notifier.rs are deliberate phase-3 stubs).
- `guests/demo`: builds with cargo-component 0.21.1 → 27KB component.
  `guests/approval`: → 24KB component.
- `scripts/accept_phase1.sh`: **PHASE 1 PASS, twice in a row** (2026-07-15), and
  again 2026-07-16 on the phase-2 tree — the durable-sleep replacement did not
  regress it (restart now sleeps only the remainder, so it completes FASTER).
  Evidence from the original run, for regression comparison:
  - engine.log shows `recovering workflow <id>` after the kill -9 restart, and the
    replayed "first fetch" guest log line lands ~100µs after workflow start (journal
    read, no network) vs the live run's ~120ms — replay path confirmed.
  - journal = exactly 4 rows: `random-u64(0), http-get(1), sleep-ms(2), http-get(3)`;
    exactly 2 http-get rows total (pre-crash call NOT re-executed).
  - workflows.output stamp == journaled random-u64 value (deterministic replay).
  - Duplicate guest log lines after restart are EXPECTED (log is not journaled, §4.1).
- `scripts/accept_phase2.sh`: **PHASE 2 PASS, twice in a row, fresh DB each time**
  (2026-07-16). Evidence from the second run:
  - Survived kill -9 at BOTH park points (waiting_event, then mid-sleep); 2×
    `recovering workflow` in engine.log.
  - `wake_at` identical before/after the mid-sleep crash (W1 == W2): the sleep
    resumed its remainder instead of restarting.
  - journal = exactly 2 rows:
    `await-event(0) {"name":"approve"} → {"payload":"{\"by\":\"alice\"}"}` and
    `sleep-ms(1) {"ms":60000} → {}` — §4.2 shapes exactly; exactly ONE await-event
    row (the crash after delivery did not re-deliver).
  - events row ends `delivered=1, delivered_seq=0` (the phase-3 un-delivery
    machinery has what it needs).
  - workflows.output = `{"approved_with":{"by":"alice"}}`; timers table empty.
  - UI smoke: dashboard, workflow page (contains full id), embedded htmx all serve.

- `scripts/accept_phase3.sh`: **PHASE 3 PASS, twice in a row, fresh DB each time**
  (2026-07-16). Evidence from the second run:
  - snapshots row present by t≈12s; journal ≤ 4 rows throughout (pruning works —
    at completion the journal is exactly ONE row, the final `checkpoint(15)`).
  - engine.log: `resuming <id> from checkpoint seq 3` TWICE — once for the kill -9
    recovery, once for the post-upgrade respawn (same C; the upgrade landed before
    the next tick's checkpoint).
  - upgrade → 200; final output `{"note":"upgraded","total":8}` — v2 parsed the
    v1 state blob and carried the tick count; `workflows.module_hash` == v2 hash;
    snapshot state ended as v2-shaped JSON.
  - upgrading the completed workflow → 409.
- After the final commit, ALL THREE suites were re-run at HEAD in sequence:
  `PHASE 1 PASS`, `PHASE 2 PASS`, `PHASE 3 PASS` — no cross-phase regressions.

Git history note: phase 1 was built in a single pass and committed per-task
afterwards; individual historical commits group files by task but were not each
built in isolation. Phases 2 and 3 WERE built task-by-task (each numbered task
compiled and was committed in order). HEAD is the verified tree.

---

## Build complete — notes for whoever works here next

The spec is fully built; there is no "next task". If you extend Keel, keep these
rails:

- **The §0 invariants above are permanent.** Journal-commit-before-return; no
  replay mode; every SQL statement in db.rs (journal.rs excepted); every
  Connection from `db::open_conn`; every workflows status write through
  `db::set_status`; `runner::spawn` only from its three call-sites (api.rs
  creation, main.rs recovery, api.rs upgrade step 5).
- **The acceptance scripts are the regression suite.** Run all three after any
  engine change; they are the definition of "still works". They need network
  (example.com), port 8080 free, and `cargo-component` on PATH (~/.cargo/bin).
- **Extending the WIT**: adding an import is source-compatible but old uploaded
  .wasm BLOBS keyed to an older interface version will not instantiate (see the
  WIT-versioning caveat above). Adding/renaming exports is breaking for all
  guests. Journal `kind`/payload shapes (§4.2) are ON-DISK format — never rename.
- **Known accepted limitations** (spec non-goals — do not "fix" casually):
  runaway guests pin their thread (epoch interruption deferred); at-least-once
  effects on crash between exec and journal INSERT; parked workflows hold
  --max-running permits; a mid-sleep upgrade restarts the sleep in full;
  no auth/TLS/clustering/metrics; guest HTTP is GET-only.
- **Upgrade machinery subtleties** live in deviations 13–15 — read them before
  touching host.rs checkpoint, the thread registry, or api.rs upgrade_workflow.

## Debugging crib

- Engine logs: stdout (acceptance scripts redirect to `engine.log`).
- Inspect a run: `sqlite3 accept1.db 'SELECT seq,kind,request,response FROM journal ORDER BY seq'`
  and `SELECT id,status,output FROM workflows`. Phase 2 state lives in
  `SELECT * FROM timers` (one row per sleeping workflow, gone after wake) and
  `SELECT name,delivered,delivered_seq FROM events`; phase 3 in
  `SELECT journal_seq, module_hash FROM snapshots` (acceptance DBs: accept1/2/3.db).
- A parked workflow reacts to events/wakes within ~1s even if the Notifier misses —
  if it doesn't, suspect a Connection opened outside db::open_conn (locked DB).
- A workflow stuck parked with NO thread after a failed upgrade = abort flag left
  set (should be impossible — the handler clears it on every path; see dev. 15).
- Consult the Troubleshooting table at the bottom of SPEC.md BEFORE improvising —
  most "weird" failures are listed there with fixes.
- Kill a stray engine: `pkill -f 'keel serve'` (acceptance scripts leave none on
  success, but a failed run can).
