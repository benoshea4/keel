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
- **Post-review hardening landed 2026-07-16** (see "Post-review hardening" below):
  unit tests, cancel endpoint + epoch interruption, upgrade pre-flight, atomic
  wake txn, panic guard, indexes, hardened scripts (offline, self-cleaning), CI.

**WHERE THINGS STAND NOW (2026-07-19, through v4.0):** everything above plus
v1.1–v2.6 (auth, effects, secrets, cron, DR, fleet cells, crate split,
providers pure/effectful/registry, kv versioning, idempotency keys, OTel),
the ENTIRE micro-cloud extension v2.7–v3.0 (serverless functions, the judge,
hosted full-stack apps), Amendment 1 as v3.1–v3.2 (rate limits off the
ledger, captured fn logs, ledger retention, the client CLI), the §P audit's
fixes as v3.3–v3.4 (sanitized public faults, global sandbox cap, bounded
compile cache, data-plane deadline; ETag/304, API+CLI symmetry, favicon,
percentiles), Amendment 2 as v3.5 (per-ref config + durable kv, WIT 0.8.0),
and Amendment 3 as v4.0 — THE ECOSYSTEM RELEASE: unmodified wasi:http/proxy
components on the same /fn routes, wasi:keyvalue on the same store, outbound
as an operator grant. **Suite = 24 gates / 53 unit tests, all in CI. Twelve
tagged releases (v1.0–v4.0), each on a CI-verified SHA with 6 verified
assets.** The full story: sections A–F, N (micro-cloud), O (Amendment 1),
P (the audit), Q (v3.3), R (the approved v3.4/v3.5/v4.0 plan + records +
ship chain), **S (the v4.1 audit + hardening slice: 13 confirmed findings
FIXED in-tree — incl. a P1 proxy-outbound permit-exhaustion — green on
build/clippy/54 unit tests/4 covering gates, NOT yet a tagged release; plus
the expanded founder shelf incl. quantum-as-a-provider + the agent runtime)**.
The remaining shelf is in §R's tail and §S — all demand-driven.

Build/run cheatsheet (run from this directory, `keel/`):

```bash
cargo build --release -p keel-engine                # engine → target/release/keel
cargo test --release -p keel-engine                 # unit tests (in-memory SQLite)
(cd guests/demo && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/approval && cargo component build --release --target wasm32-unknown-unknown)
(cd guests/counter && cargo component build --release --target wasm32-unknown-unknown)  # +--features v2
(cd guests/spin && cargo component build --release --target wasm32-unknown-unknown)
./scripts/accept_phase1.sh                          # must print PHASE 1 PASS (offline — local stub)
./scripts/accept_phase2.sh                          # must print PHASE 2 PASS
./scripts/accept_phase3.sh                          # must print PHASE 3 PASS
./scripts/smoke_cancel.sh                           # must print CANCEL SMOKE PASS
./scripts/smoke_auth.sh                             # must print AUTH+LIMITS SMOKE PASS
./scripts/smoke_effects.sh                          # must print EFFECTS SMOKE PASS (v1.2/v1.3 + v2.1 cron/timeout)
./scripts/smoke_dr.sh                               # must print DR SMOKE PASS (backup/restore)
./scripts/smoke_fleet.sh                            # must print FLEET SMOKE PASS (cell tenancy)
./scripts/smoke_secrets.sh                          # must print SECRETS SMOKE PASS (v2.1)
./scripts/smoke_embedded.sh                         # must print EMBEDDED SMOKE PASS (v2.2 crate split)
./scripts/smoke_providers.sh                        # must print PROVIDERS SMOKE PASS (v2.2)
./scripts/smoke_kv_upgrade.sh                       # must print KV-UPGRADE SMOKE PASS (v2.3)
./scripts/load_test.sh                              # must print LOAD TEST PASS (v2.4, 200 wf / cap 8)
./scripts/smoke_providers_effectful.sh              # must print EFFECTFUL PROVIDERS SMOKE PASS (v2.5)
./scripts/smoke_provider_registry.sh                # must print PROVIDER REGISTRY SMOKE PASS (v2.6)
./scripts/accept_phase4.sh                          # must print PHASE 4 PASS (micro-cloud functions, v2.7)
./scripts/accept_phase5.sh                          # must print PHASE 5 PASS (judge + metering + watchdog, v2.8)
./scripts/accept_phase6.sh                          # must print PHASE 6 PASS (hosted apps, v3.0; needs trunk)
./scripts/accept_operate.sh                         # must print OPERATE PASS (rate limits/logs/retention, v3.1)
./scripts/accept_cli.sh                             # must print CLI PASS (client verbs, v3.2)
./scripts/accept_harden.sh                          # must print HARDEN PASS (caps/sanitized faults/timeouts, v3.3)
./scripts/accept_polish.sh                          # must print POLISH PASS (ETag/CLI symmetry/favicon/percentiles, v3.4)
./scripts/accept_functions2.sh                      # must print FUNCTIONS2 PASS (config + durable kv, WIT 0.8.0, v3.5)
./scripts/accept_ecosystem.sh                       # must print ECOSYSTEM PASS (wasi:http/proxy + wasi:keyvalue, v4.0)
./scripts/accept_hardv41.sh                          # must print HARDV41 PASS (§S audit fixes: proxy-outbound bound, metrics escaping, config cap; needs a free :18080)
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

## Post-review hardening (2026-07-16, after the spec was fully built)

An adversarial code review of the finished build demanded six fixes; all landed.
These are review-driven changes, not spec tasks — where they touch spec-verbatim
material it is noted.

A. **Scripts self-clean and run offline.** All acceptance scripts now: `trap`
   cleanup (a FAILING run no longer leaks a live server holding :8080 that the
   next run's curls would silently hit), a :8080 preflight, readiness polling
   after every engine launch (no more `sleep 1` startup races), and phase 1
   fetches a LOCAL stub (`python3 -m http.server` on :18080 serving
   `scripts/stub/`) instead of example.com — the demo guest now reads its fetch
   url from the workflow input (`{"url": ...}`, defaulting to example.com for
   bare runs). The phase-1 script is therefore no longer spec-verbatim: the
   ASSERTIONS are unchanged; only the harness was hardened.
B. **Upload validation + upgrade pre-flight (the brick fix).** `/api/modules`
   rejects bodies without the `\0asm` magic (400). The upgrade handler runs
   `runner::preflight` (compile + `Linker::instantiate_pre` import check +
   `WorkflowPre::new` export/type check — no guest code runs) BEFORE the
   destructive tail-discard txn. Previously, upgrading a healthy workflow to a
   module that couldn't start discarded its tail and then failed at respawn —
   permanently, since failed is terminal.
C. **Unit tests** (`cargo test -p keel-engine`, in-memory SQLite): journaled()
   live/replay/kind-mismatch/request-mismatch/failed-exec, event delivery
   (oldest-first, exactly-once, in-txn status flip), `finish_sleep`,
   `upgrade_module_txn` (tail discard + event UN-delivery + repoint), and
   `finish_cancel`. The un-delivery UPDATE now runs under a test.
D. **Atomic wake transactions.** The sleep wake-up (timer delete + journal
   insert + status flip) is ONE txn — `db::finish_sleep` — closing the crash
   window that silently turned remainder-sleep into full re-sleep. The
   await-event delivery txn also flips status running inside the txn now.
   (`set_status` remains the only status writer — it runs against the txn.)
E. **Cancel endpoint + epoch interruption.** `POST /api/workflows/{id}/cancel`
   → 200, workflow becomes `failed` with output `cancelled by operator`
   (timer cleaned in the same txn — `db::finish_cancel`); terminal workflows
   409. Parked workflows abort via the park loops; guests spinning in PURE WASM
   are trapped by the epoch-deadline callback (engine ticks 1/s; the callback
   re-arms unless the abort flag is set, then raises AbortForUpgrade through
   the same silent-exit chain). Cancel shares the upgrade claim set (no
   interleaving) and `abort_and_join` (api.rs) is the shared bounded-join.
   `guests/spin` (a `loop {}` guest) exists to regression-test this.
F5. *(v1.1, same day)* **Auth + guest memory limits** — the go-public
   prerequisites. `--api-token` / `KEEL_API_TOKEN` (unset = v1.0 open mode with
   a startup warning): auth.rs middleware wraps the whole router, allowlisting
   /assets/*, /login, /logout; API callers send `Authorization: Bearer`, the UI
   logs in at /login and gets an HttpOnly SameSite=Lax cookie carrying
   hex(sha256(token)) — never the raw token; comparisons are hash-then-compare
   (timing-safe without a constant-time dep). `--max-guest-memory-mb` (default
   256) builds a wasmtime StoreLimits per store; a guest that outgrows it
   fails. `scripts/smoke_auth.sh` gates all of it (401s, bearer lifecycle,
   cookie digest, open-mode regression, 1 MiB cap → failed).

G. *(v1.2/v1.3/v2 slice, 2026-07-16 — "build out the full roadmap")* One pass,
   each piece gated by a script:
   - **WIT 0.4.0**: `http-request` (method/headers/body/opt-in retries; non-2xx
     is DATA, journal kind `http-request`), `kv-set`/`kv-get` (kind `kv-set`/
     `kv-get`; write+journal and read+journal are single transactions in db.rs,
     same discipline as event delivery). All five guests rebuilt; new
     `guests/effects` exercises everything; `scripts/stub_server.py` echoes and
     COUNTS posts so `smoke_effects.sh` can prove replay ≠ re-execution.
   - **Interval schedules**: `schedules` table + a 1s scheduler loop in main.rs
     — runner::spawn call-site **4 of 4** (the §0 "exactly three" rule got its
     one sanctioned amendment; comments updated). Missed windows collapse into
     one firing (`advance_schedule` math).
   - **v1.3 operability**: `GET /api/workflows` paged list, `/metrics`
     (hand-rolled Prometheus text), retention GC (`--retain-terminal-hours`,
     sweeps at startup then every 60s, cascades journal/events/snapshots/kv),
     UI logout link.
   - **v2 DR**: `db::backup_to` (rusqlite backup API — consistent while live),
     `--backup-dir/--backup-interval-secs/--backup-keep` loop with pruning,
     `keel backup` one-shot. `smoke_dr.sh` deletes the db and restores from a
     snapshot mid-workflow. Continuous replication = Litestream, documented,
     deliberately not reimplemented.
   - **v2 cell tenancy**: `keel fleet --config fleet.toml` (engine/src/fleet.rs)
     — per-tenant process/db/token children (token via env, never argv),
     per-tenant `keel-<name>.log`, 1s supervision with crash-only restarts.
     `smoke_fleet.sh` proves token isolation and respawn-with-state. RLS is
     moot under cells (ROADMAP.md "Answered design questions").
   - **KV caveat** (documented in docs/guests.md, roadmapped v2.3): upgrade
     tail-discard does not roll back kv writes from the discarded tail.
   - **Docs**: ROADMAP.md + docs/{operations,api,guests}.md.

H. *(v2.1, 2026-07-16 — secrets / cron / timeout, WIT 0.5.0)*
   - **`secret(name)`** (engine/src/host.rs, hand-rolled journaling): journal
     row = `{"name"}` → `{"salt","sha256"}` — the VALUE never touches SQLite.
     Replay re-reads the live `--secrets-file` and verifies the salted hash;
     a mismatch traps with "changed mid-workflow" (workflow → failed, the
     operator restores the value or cancels). An error result (`{"err"}`) is
     journaled and replays as the same error. Read values are REDACTED from
     journaled http-request/http-get requests (`{{secret:name}}` placeholder;
     wire carries real bytes) via `Ctx.read_secrets` — every distinct
     (name, value) pair read this execution, which is replay-deterministic
     because secret() hash-checks first. Redaction CANNOT cover checkpoint
     state, kv values, event payloads, or response bodies — documented sharp
     edge in docs/guests.md. Secrets file: KEY=VALUE, strict parse (no
     '='-less lines, no duplicate keys), validated at startup (fail fast),
     0600 warning, re-read on every call so rotation is live.
   - **Cron schedules** (engine/src/cron.rs): hand-rolled 6-field parser
     (sec min hour dom mon dow; UTC; vixie dom/dow-OR; bitmask sets +
     Hinnant civil_from_days — no chrono). `cron::next_run` is the ONE
     "when next" decision for both kinds; `db::fire_schedule` now takes the
     caller-computed next_run_at inside the same single txn (advance_schedule
     SQL is gone). A schedule whose expression can never fire again is
     auto-disabled by the scheduler (not hot-looped). `PATCH
     /api/schedules/{id}` {"enabled"} pauses/resumes. Schema: `schedules.cron`
     TEXT NULL via `ensure_column` (the one sanctioned additive ALTER —
     pre-v2.1 DBs retrofit at startup; unit-tested).
   - **`timeout-ms` on http-request**: caps each ATTEMPT (0 = 30s agent
     default); a timeout is a transport failure (retryable if opted in).
   - Journal request JSON for http-request grew a `timeout_ms` field —
     workflows in flight across the 0.4→0.5 engine upgrade were already
     rebuild-gated by the WIT bump (blob-incompatible), same policy as v2.0.
   - Gates: `smoke_secrets.sh` (placeholder in journal + real value on the
     wire + replay across kill -9 + loud rotation failure + no raw bytes
     anywhere in db files or engine.log), extended `smoke_effects.sh` (cron
     fires ≥2 in 6s, PATCH pause holds, 400s for cron+interval both/junk
     cron, `slow_timed_out` via stub /slow at 3s vs 300ms budget). 27 unit
     tests (cron parser/next-fire, secrets parse/redact, fire_schedule txn,
     ensure_column retrofit).

I. *(v2.2, 2026-07-16 — crate split + capability providers, WIT 0.6.0)*
   - **keel-core** (core/): db, journal, notifier, host, runner, cron,
     provider moved out of the binary crate VERBATIM (git mv; in-crate
     `crate::` paths unchanged). The binary keeps main/api/auth/ui/fleet and
     consumes `keel_core::*`; rusqlite is re-exported from keel-core so the
     workspace can never carry two versions. `Engine` façade (lib.rs):
     `EngineOptions::new(db)` + `Engine::open` (open+migrate+RECOVERY — the
     binary and embedders share the startup path), `upload_module`,
     `start_workflow`, `workflow`. EngineShared::new now takes EngineOptions.
     Spawn call-sites stay EXACTLY FOUR: 1 start_workflow (lib — api.rs's
     create_workflow now goes through it), 2 Engine::open recovery (lib),
     3 upgrade step 5 (bin), 4 scheduler (bin). Upgrade/cancel orchestration
     deliberately stays bin-side (documented in lib.rs).
   - **Providers** (core/src/provider.rs + provider-wit/ + PROVIDERS.md):
     import-free `keel:provider` components (`handle(kind, request) →
     result<json, err>`), registered `--provider name=path.wasm` ([a-z0-9-]
     names, compiled + instantiate_pre + export-typed at BOOT — bad provider
     = failed start), per-tenant `providers = [...]` in fleet configs.
     provider-call journals `custom:<name>:<kind>` with the request REDACTED
     (secrets) — replay never re-instantiates (gate counts the live-path
     "invoking provider" log line). Bounds: guest memory cap + ~10-tick
     epoch budget (a spinning provider is an err, never a pinned worker).
     Unknown name/kind/trap/budget = guest-visible err, journaled as data
     (stays an err on replay even if the provider is later registered).
   - Gates: `smoke_embedded.sh` (counter runs in-process via
     core/tests/embedded.rs; examples/embedded.rs compiles in the same
     invocation), `smoke_providers.sh` (boot refusal on junk provider;
     replay-not-reinvoked across kill -9; custom:* journal kinds;
     unregistered provider errs as data), `smoke_fleet.sh` extended
     (provider on tenant A completes, same guest on tenant B fails with
     "no provider 'greet'"). CI lint/test now --workspace.

J. *(v2.3, 2026-07-16 — kv versioning + idempotency keys; NO WIT bump)*
   - **KV is append-only versions**: `kv (workflow_id, key, seq, value)`,
     seq = the writing kv-set's journal seq. Live reads take the highest
     version (live reads only happen at the execution head, so highest ≡
     current); `upgrade_module_txn` deletes versions with seq > C together
     with the journal tail — THE kv-vs-upgrade caveat from G is CLOSED
     (guests.md rewritten). `snapshot_and_prune` compacts superseded
     versions at each checkpoint (safe: the surviving latest version is
     always ≤ the snapshot C any future upgrade will use). Pre-v2.3 tables
     are reshaped once in migrate() (has_column check; old rows become
     version 0 = "pre-checkpoint"). `db::kv_latest` added for the v2.4 UI.
   - **Idempotency keys, wire-only**: http-request sends
     `keel-idempotency-key: <workflow_id>:<seq>` (the seq the call is about
     to claim — stable across replay AND the crash-and-resend window).
     Injected into the WIRE headers only; the journaled request keeps
     exactly what the guest passed, so pre-v2.3 journals replay unchanged
     and the key stays derivable from the row. Guest-supplied header wins;
     empty value = send none. http-get carries none (legacy).
   - Gates: `smoke_kv_upgrade.sh` (kvup→kvup2: two versions before upgrade,
     tail version discarded by it, v2 resume reads "below", exactly one
     surviving version), `smoke_effects.sh` extended (stub-observed key ==
     "<wf>:1", journal http-request rows never contain the header, kv
     version count assertions). 31 unit tests (kv versioned read/upgrade
     discard, checkpoint compaction, pre-v2.3 reshape, key injection
     inject/override/suppress).

K. *(v2.4, 2026-07-16 — surface & scale polish; NO WIT bump)*
   - **UI**: /schedules page (create form with interval-or-cron, 2s-polled
     table, Pause/Resume via hx-patch + Delete via hx-delete — both endpoints
     grew urlencoded form shapes alongside JSON for this) and a "Durable KV"
     section on the workflow detail (db::kv_latest, truncated like journal
     cells, hidden when empty). Schedules nav link on every page.
   - **keel_active_permits** metric: threads actually holding a --max-running
     permit (keel_worker_threads counts permit-WAITERS too — that distinction
     is what makes the cap observable at all).
   - **load_test.sh**: 200 loadgen workflows (new minimal guest: one http-get,
     NO sleeps — the first draft used the demo guest and its 15s sleep turned
     the gate into a nap-schedule benchmark, 32/200 in 60s; permits are held
     while parked BY DESIGN) through --max-running 8 against stub /slow?ms=300.
     Asserts: all 200 complete, permit gauge sampled every 200ms never exceeds
     8 AND reaches exactly 8, per-workflow journals dense (COUNT == MAX+1),
     exactly 200 http-get rows.
   - **OTel behind `--features otel`** (binary feature; default build carries
     zero OTel deps): span per workflow execution (runner.rs) with a child
     span per journaled host call (journal.rs), exported OTLP/http via the
     standard OTEL_* env. Side effect on ALL builds: fmt log lines now carry
     span context (workflow{id=..}: host_call{kind=.. seq=..}:) — grep-based
     gates unaffected (substring matches). Best-effort by design (kill -9
     drops unexported spans). CI compile-gates the feature.
   - **linux-arm64 release binaries** (aarch64-unknown-linux-gnu on
     ubuntu-24.04-arm runners, free for public repos) — releases now carry 6
     assets.
   - **Deploy recipes** (docs/deploy/): systemd unit (EnvironmentFile token,
     SIGKILL stop — hard kills are supported), 2-stage Dockerfile +
     compose.yml, single-replica k8s (replicas MUST stay 1 per db; Recreate
     strategy; scale = more cells).

L. *(v2.5, 2026-07-17 — EFFECTFUL providers; keel:provider 0.2.0, guest WIT
   untouched — no guest rebuilds)*
   - **The seq-allocation design** (the open problem PROVIDERS.md v2.2 flagged):
     an effectful provider's wire calls journal as ORDINARY rows (kind
     `provider-http:<name>`) at the calling workflow's own dense seqs, through
     a NESTED OWNED JournalCtx — fresh Connection to the same db, cursor
     copied in from Ctx.j.next_seq and copied back out after the call. Safe
     because the guest is suspended inside provider-call for the whole
     duration (the two connections never write concurrently). The
     `custom:<name>:<kind>` terminal row commits LAST, at the seq after the
     internals. Dense-seq, commit-before-return and no-replay-mode all hold
     per wire call; journaled()'s nondeterminism trap now reaches INSIDE
     providers for free.
   - **Replay** (host.rs scan_provider_scope, unit-tested): from the cursor,
     rows of `provider-http:<name>` belong to the scope; the first
     `custom:<name>:<kind>` row is the terminal → fast-forward past the whole
     scope WITHOUT instantiating the provider (request compared like
     journaled() does). No terminal (crash mid-provider) → re-invoke live:
     recorded wire calls replay inside the nested journaled() (never
     re-fired), the in-flight one re-sends with the SAME wfid:seq idempotency
     key — each wire call has its own seq, so v2.3's key mechanism composed
     with zero new code. Any foreign kind in the scope → nondeterminism bail.
   - **Tier is an operator grant**: --provider keeps the v2.2 import-free
     preflight verbatim (effectful components REJECTED at boot);
     --provider-effectful preflights against a linker exporting exactly
     keel:provider/host-http (pure components pass — grant ⊇ use). One name
     namespace, dup-checked across tiers. Fleet: per-tenant
     providers_effectful. ProviderEntry{component, effectful} in EngineShared.
   - **Failure taxonomy exception (deliberate)**: provider failures stay DATA
     (trap/budget/instantiation/transport → journaled err), but a
     journal-integrity failure INSIDE an import (nondeterministic provider
     re-run, db error) must fail the WORKFLOW — EffCtx carries it out in a
     `fatal` slot (the wasm trap is just the vehicle) and call_effectful
     returns it as the OUTER error, which host.rs traps the guest with.
     Laundering nondeterminism into a journaled err would corrupt silently.
   - **Epoch budget rethink**: pure providers keep set_epoch_deadline(10), no
     callback. Effectful stores set deadline 1 + a callback, because epoch
     ticks accumulate during a slow wire call and would charge WALL time
     against the wasm budget (an 11s POST would trap the provider on the next
     wasm instruction). The callback excuses exactly one firing per completed
     host call (EffCtx.returned_from_host, set after journaled() returns) and
     counts the rest — ~10 ticks of pure-wasm spin budget, wire time bounded
     by http timeout/retry caps instead. The gate's 11s /hook/b resend proves
     the excusal works.
   - **WIT**: keel:provider 0.2.0 = old `world provider` TEXTUALLY UNCHANGED
     (0.1.0-built pure providers keep working — export types are checked
     structurally, not by version string) + new `world provider-effectful`
     (import host-http: http-request mirroring the guest signature
     field-for-field). Second bindgen! in provider.rs (imports default
     trappable). keel:workflow stays 0.6.0 — provider-call is unchanged, so
     NO guest rebuilds; v2.5 is a drop-in engine swap.
   - **Redaction reach**: the provider's journaled wire requests redact with
     the CALLING execution's read_secrets (cloned into EffCtx) — a secret the
     guest passed into a provider request cannot leak raw through the
     provider's rows either.
   - **Samples + gate**: providers/relay (POST first_url then second_url via
     the import), guests/relay (one provider-call + 5s sleep).
     smoke_providers_effectful.sh proves: effectful-under---provider boot
     REFUSAL; kill -9 mid-second-POST → /hook/a hit ONCE, /hook/b hit twice
     with the SAME key both times (stub now records per-path hits+keys at
     ARRIVAL, /hits endpoint), "invoking provider" == 2 (per-effect journaling
     re-invokes after a mid-scope crash — boundary journaling would show 1);
     kill -9 post-scope → fast-forward, count stays put; pure greet under the
     effectful grant → works, zero internals. Journals dense throughout.
   - **Sharp edges documented** (PROVIDERS.md rules): don't change a
     provider's tier mid-workflow (effectful history under a pure grant trips
     the nondeterminism trap); cancel latency window = sum of the provider's
     wire timeouts; provider responses journal verbatim (don't echo secrets).

M. *(v2.6, 2026-07-18 — content-addressed provider registry; NO WIT change)*
   - **Schema**: provider_blobs(hash PK, wasm, created_at) — immutable,
     sha256-keyed — + providers(name PK, effectful, hash FK, updated_at), the
     mutable pointers. Additive CREATE IF NOT EXISTS (old DBs just gain empty
     tables). db.rs: upsert_provider (blob INSERT OR IGNORE + binding upsert,
     one txn), get_provider_blob, list_providers, delete_provider,
     load_provider_registry; registry roundtrip unit test (35 total now).
   - **EngineShared.providers became Arc<RwLock<HashMap>>** (was immutable
     Arc<HashMap>): provider_call snapshots (tier, component) under a read
     lock then runs lock-free — a mid-call mutation affects the NEXT call.
     provider_call dispatch unchanged otherwise.
   - **Boot semantics (deliberate behavior change, documented)**: flags
     validate EAGERLY (bad flag still fails boot — the v2.2 promise), then
     UPSERT into the db registry → flag providers now PERSIST across
     restarts; removal is DELETE /api/providers/{name}. Then every stored
     binding loads; a blob that stops compiling (e.g. future wasmtime bump)
     logs + skips (name acts unregistered, journaled err) — no bricked boots
     from stored state, uploads were pre-flighted anyway.
   - **API** (api.rs, all behind the bearer token): POST /api/providers
     (raw/multipart/rebind-by-hash shapes; tier REQUIRED — it is an operator
     grant; per-tier preflight → 400 at the door), GET list, DELETE unbind.
     lib.rs façade: upload_provider / rebind_provider (preflight BEFORE the
     pointer moves) / remove_provider / list_providers — embedders get the
     same registry. /providers UI page (upload form + bindings table + delete)
     and a nav link on every page.
   - **Replay-vs-registry rule**: recorded rows win in all three dispatch
     arms, so rolling a provider never rewrites history — gated by killing a
     workflow mid-sleep, re-uploading greet-v2 (greet grew a `v2` feature
     flag, the counter-guest trick), restarting FLAGLESS: the recovered
     workflow's output has the RECORDED v1 greeting, a new workflow gets v2,
     and a rebind to the v1 hash (no bytes) rolls back. Sharp edge documented:
     rolling mid-EFFECTFUL-call can trip the nondeterminism trap on recovery
     (new version ≠ recorded wire calls) — roll when calls aren't in flight.
   - **Gate**: smoke_provider_registry.sh (15 gates total) — also covers junk
     → 400, effectful-under-tier=pure → 400, DELETE → workflow fails with
     "no provider 'greet'" as data, flag-boot upsert visible in GET list, UI
     page lists bindings. Lesson recorded: serde_json sorts object keys, so
     the gate asserts via python dict parsing, not field-order greps.

F. **Follow-ups from the review.** Panic guard in runner::spawn (catch_unwind →
   failed status + registry/notifier cleanup on the panic path; poison-tolerant
   locking on the exit path and Permit::drop). Two indexes (events park-loop
   lookup, workflows created_at). Notifier entries are removed at thread exit
   (the map no longer grows forever). The workflow page surfaces API error
   bodies from its hx-swap="none" forms (htmx responseError/sendError listeners
   + an `.error` box). CI (`.github/workflows/ci.yml`): clippy -D warnings +
   unit tests, then all four scripts on ubuntu-latest.

One WIT-versioning caveat for later phases: bumping the package to 0.2.0 required
REBUILDING both guests (bindings.rs is generated from the wit). "Adding an import is
non-breaking" is a source-level promise — an old `.wasm` BLOB compiled against
`host-api@0.1.0` will not instantiate against a linker that only exports 0.2.0
(0.x minors are semver-incompatible). Fresh-DB acceptance never sees this, but a
long-lived deployment upgrading the engine under stored modules would. Phase 3 bumps
to 0.3.0 and is breaking by design (adds a `resume` export) — all guests get a stub
`resume` then, per Task 3.1.

N. **Micro-cloud extension (phases 4–6) — reconciliation plan + build log
   (started 2026-07-18).** Source spec:
   `../keel-microcloud-extension-spec.md` rev 1.0 (functions → sandbox/judge →
   hosted apps; "one binary micro-cloud"). It was written against the BASE
   spec's world (phases 1–3, WIT 0.3.0, no providers/auth/schedules/registry),
   so where its letter conflicts with shipped v2.6 reality, these resolutions
   govern (spirit kept; each marked in code):
   1. **WIT bump is 0.6.0 → 0.7.0** (not "0.4.0" — that version shipped in
      v1.2). Package `keel:workflow@0.7.0` adds `interface platform-api`
      (log, now-ms, random-u64, start-workflow, get-workflow), `world handler`
      (imports platform-api; the http-request/http-response records are
      declared INLINE in the world — WIT has no package-level types, the ext
      spec's snippet placement isn't valid syntax), `world solver` (zero
      imports — tightest sandbox). `world workflow` text unchanged. Engine
      grows bindgen #3 (handler) + #4 (solver) in separate rust modules
      (provider.rs already set this pattern).
   2. **Ticker 1s → 100ms.** Epochs + a ticker already exist (cancel v1.x,
      provider budgets v2.2/v2.5). Rescale: pure provider deadline 10→100
      ticks, effectful ticks_used budget 10→100 (both still "~10s of wasm
      time"); workflow cancel callback unchanged (re-arm Continue(1) each
      tick; worst-case cancel latency IMPROVES 1s→100ms). Function/solver
      deadlines = ceil(time_limit_ms/100) + epoch_deadline_trap (no callback).
   3. **Fuel engine-wide**: `consume_fuel(true)` on the ONE Engine. Every
      store sets fuel or instantly traps at 0: workflow stores =
      `--wf-fuel-limit` (default 10^13; new EngineOptions.wf_fuel_limit;
      OutOfFuel → status failed, output "runaway guest: exhausted compute
      budget"); provider stores (pure + effectful — the ext spec predates
      providers) = 10^13 constant backstop, epoch budget stays their primary
      containment; function/solver stores = per-route / judge quotas (the
      real metering). Workflow epoch profile is NOT the ext spec's
      deadline(10^12)+trap: our deadline(1)+abort-checking callback IS the
      cancel mechanism (base gates require it) and already guarantees epochs
      never kill a healthy workflow.
   4. **runner::spawn call-sites: now FIVE** (creation, recovery, upgrade,
      schedules v2.0, platform-api start-workflow). Ext spec says "four"
      because it predates schedules. Every "exactly 4" comment updated.
   5. **Auth boundary**: v1.1 token auth exists; ext spec declares auth a
      non-goal for new surfaces. Resolution: CONTROL plane (/api/routes,
      /api/problems, /api/submissions, /api/apps, new UI pages) sits behind
      the existing middleware like everything else; DATA plane (/fn/*,
      /apps/* serving) is mounted AFTER the auth layer = public by design — a
      browser-served app must call its own backend tokenless. In
      operations.md.
   6. **Placement per the v2.2 crate split**: core/src/sandbox.rs (MemLimiter,
      Outcome, classify — normative order: mle→oof→tle→trap→ok),
      core/src/function.rs (handler bindgen, FnCtx, platform-api host impl,
      invoke_handler — ledger write INSIDE so metering can't be forgotten),
      core/src/judge.rs (solver bindgen, per-case constants,
      judge_submission). Binary: api.rs CRUD, dispatch.rs (/fn + /apps
      serving), ui.rs + templates (routes/playground/problem/usage/apps),
      main.rs wiring. db.rs: E3 tables appended to MIGRATION + accessors.
      The {id,status,output} workflow JSON is shared via a core helper used
      by BOTH api::get_workflow and platform-api get-workflow.
   7. **Guests**: echo-fn + starter-fn (world handler); sum-solver (+`wrong`
      feature), loop-solver, hog-solver (world solver); the runaway-workflow
      fixture is the EXISTING guests/spin (already `loop {}`). apps/hello =
      Leptos 0.7 CSR via Trunk (phase 6; trunk not installed yet — cargo
      install locally, taiki-e/install-action in CI; fallback ladder per
      spec Task 6.2).
   8. **Ship cadence**: one commit per task (§0), per phase: angry
      self-review → full gate suite (grows 15→18) → push → CI → tag → release
      with hand-written notes. Tags: phase 4 = v2.7, phase 5 = v2.8,
      phase 6 = **v3.0** (the micro-cloud completion is the 3.0 story).
   9. `invocations.ref` column name is fine (REF isn't reserved in SQLite).

   PROGRESS (tick after every task — this is the compact-resume pointer):
   - [x] 4.1 consume_fuel + 100ms retick + --wf-fuel-limit + runaway retrofit; base gates re-run
   - [x] 4.2 WIT 0.7.0 + handler/solver worlds + platform-api host impl (FnCtx)
   - [x] 4.3 routes table/API + /fn dispatcher + classify() + ledger
   - [x] 4.4 guests echo-fn + starter-fn
   - [x] 4.5 /routes UI + nav links (Routes/Playground/Apps/Usage) on all pages
   - [x] 4.6 accept_phase4.sh ×2 + FULL suite + angry review + ship v2.7
   - [x] 5.1 MemLimiter wired on fn/solver stores
   - [x] 5.2 judge.rs
   - [x] 5.3 problems API + playground UI
   - [x] 5.4 /usage page
   - [x] 5.5 solver guests
   - [x] 5.6 accept_phase5.sh ×2 + FULL suite + angry review + ship v2.8
   - [x] 6.1 apps/assets tables + zip upload (zip-slip reject) + serving fallback
   - [x] 6.2 apps/hello (Leptos+Trunk)
   - [x] 6.3 /apps UI
   - [x] 6.4 accept_phase6.sh ×2 + FULL suite + angry review + ship v3.0
   - NEXT: 6.4 finale — full 18-gate suite, push, CI, tag v3.0 + release.
     After that the MICRO-CLOUD EXTENSION IS COMPLETE; further work is
     demand-driven only (ROADMAP Next: host-kv, replication, stretch items).

   PHASE 6 RECORD (v3.0, 2026-07-18):
   - **Serving (6.1)**: one wildcard route (/apps/{*full}), name parsed
     manually; order = index.html → exact asset → api/* backend (same
     dispatch core as /fn/*, kind 'app', routes-table default quotas) → SPA
     fallback for extensionless paths. Assets live in SQLite with stored
     content types (.wasm/.js FORCED — mime_guess is trusted for the rest),
     cache-control: no-store. There is NO filesystem serving, so zip-slip is
     purely an upload-time concern.
   - **Upload (6.1)**: all-or-nothing — every entry validated before any row
     lands. Angry review added two real fixes: (a) zip-BOMB cap (a 305KB zip
     decompressing to 300MB → 400; 256 MiB decompressed ceiling, header
     claim AND actual bytes both counted); (b) bare /apps/<name> 301→…/
     (serving index.html at the bare path would break its relative ./x.js
     URLs — the browser resolves them against /apps/).
   - **hello app (6.2)**: Leptos 0.7 CSR + Trunk 0.21.14 (installed via
     cargo install --locked trunk; CI gets it from taiki-e/install-action).
     leptos 0.7 API held (signal()/mount_to_body/spawn_local) — the spec's
     fallback ladder was not needed. gloo-timers (futures) added for the 1s
     poll (the spec's dep list implies it; no other wasm sleep exists).
     MODULE_HASH is sed-injected by the gate and RESTORED via trap (repo
     stays clean even on failure).
   - **Environment**: trunk 0.21.14 now installed on this machine.
   - **Gate**: PHASE 6 PASS ×2 from clean (stored>=3, root serves <script +
     .wasm, application/wasm content type + >100KB asset, THE VERTICAL SLICE
     via the app's own endpoints — start through the backend, poll through
     the backend, count:3 — zip-slip 400 with zero rows). Human check
     printed, not automated. Full 18-script suite green after (see below).
     HUMAN CHECK PASSED 2026-07-19: the user clicked "Start job" in a real
     browser and pasted back "workflow 87ae60ad…: completed / {"count":3}".
     It first surfaced a REAL bug the gate missed — trunk's default ABSOLUTE
     asset URLs (/hello-*.js) 404 under the /apps/<name>/ mount → blank
     page. Fixed with Trunk.toml public_url = "./"; the gate now rejects
     root-absolute refs and fetches the JS through the app path (cdeacbd).

   PHASE 5 RECORD (v2.8, 2026-07-18):
   - **Judge (5.2)**: judge.rs = bindgen #4 (world solver, EMPTY linker — the
     sandbox is "cannot name a capability"); per-case 10^9 fuel / 256 MiB /
     2000ms consts; classify() reused verbatim; guest-returned Err → RE with
     ledger outcome guest_error; stop at first non-AC; ONE verdict UPDATE.
   - **THE loop-solver LESSON**: a bare `loop {}` is OOF fodder (burns 1e9
     fuel in <1s), and the first "unoptimizable" memcpy loop got REDUCED TO
     `loop {}` by LLVM (copying 1s over 1s then testing for 0 is provably
     unobservable — the 64MB buffer never even allocated; caught because the
     ledger showed peak_mem 1.1MB with 1e9 fuel in 813ms). Fix: black_box on
     both sides of an 8MiB copy_within → memory.copy is ~1 fuel but real
     wall time → TLE ×3 with byte-identical fuel (802) — fuel determinism
     visible in the ledger. Diagnosing from the ledger (fuel/peak/ms) rather
     than the verdict is exactly what the meter is FOR.
   - **5.3/5.4**: dual-shape POST /api/submissions (JSON or multipart
     upload-and-judge in one call); verdict badges map onto the FIVE existing
     status classes (style.css says resist additions): AC=completed
     WA=running TLE=sleeping MLE=waiting_event RE/OOF=failed; /usage =
     totals-by-module + newest 100, polled.
   - **Gate**: PHASE 5 PASS ×2 from clean — AC (detail: 2 cases, fuel>0,
     2 solve ledger rows), WA, TLE resolves ~2s, MLE, function-side oof via
     starved rebind (fuel_limit=1000 → 500 {"outcome":"oof"} + ledger row),
     spin workflow under --wf-fuel-limit 10^7 → failed "compute budget" with
     the engine still healthy. Full 17-script suite green after. The suite
     must run on an IDLE machine: a parallel `cargo install trunk` compile
     pegged the cores and blew smoke_providers_effectful's timing windows
     (standalone rerun passed) — never overlap heavy builds with the gates.

   PHASE 4 RECORD (v2.7, 2026-07-18) — all six tasks landed, one commit each:
   - **Fuel + retick (4.1)**: consume_fuel(true) on the one Engine; ticker
     1s→100ms; provider budgets rescaled ×10 (semantics unchanged, "~10s");
     provider stores get a PROVIDER_FUEL=10^13 backstop (spec predates
     providers); workflow OutOfFuel → failed "runaway guest: exhausted
     compute budget" checked BEFORE the generic trap arm. Cancel callback
     kept (deviation from ext spec's deadline(10^12)+trap — cancel needs the
     callback; effect identical, and cancel latency improved to 100ms).
   - **WIT 0.7.0 (4.2)**: platform-api interface + handler world (records
     INLINE in the world — package-level types aren't valid WIT; the ext
     spec's snippet placement adjusted) + solver world. Existing guests
     rebuild untouched. db::workflow_json shared by API + get-workflow.
   - **Dispatcher (4.3)**: longest-prefix match on SEGMENT BOUNDARIES
     (/fn/echo never captures /fn/echo2 — the spec's "ensure leading /"
     case is vacuous by construction); 10 MiB cap via manual to_bytes (the
     2MB axum default does NOT apply to raw-body reads — verified 3MB ok,
     11MB→413); ledger row inside invoke_handler so no caller can forget
     it. Engine-level failures (module missing/compile error) are 500s
     WITHOUT ledger rows — the ledger records sandbox invocations, not
     engine faults (deliberate reading of "always insert").
   - **MemLimiter wired EARLY (5.1 pulled into 4.2)**: function stores
     without any limiter would have UNLIMITED memory — worse than early
     wiring. Task 5.1 is now a verification checkbox.
   - **Angry-review probes (all pass, live)**: control plane 401 tokenless
     while data plane (/fn/*) stays 200 WITH a token set (documented in
     operations.md — deliberate; don't bind what you wouldn't expose);
     exact-prefix hit gives guest path "/"; boundary 404; body caps.
   - **Lesson**: `kill %1` is a NO-OP in non-interactive shells (job control
     off) — probes left zombie engines holding :8080 twice. Kill by PID ($!)
     or lsof -ti :8080 | xargs kill -9. The gate scripts already do this.
   - **Gates**: PHASE 4 PASS ×2 from clean; FULL 16-script suite green after
     (accept_phase4.sh joined CI); clippy -D warnings; 37 unit tests (2 new:
     classify order, limiter peak/deny).

O. **Amendment 1 — operating the public plane + the CLI (v3.1/v3.2, started
   2026-07-19).** User go-ahead: "continue with the build... what else do we
   need for this to be an end to end build... you decide and build". Governing
   doc: [SPEC-AMENDMENT-1.md](SPEC-AMENDMENT-1.md), authored in-repo per the
   stretch-direction discipline ("spec amendment first, code second" —
   SPEC-MICROCLOUD.md; SPEC-MICROCLOUD.md itself stays a pristine copy).
   Two stages: v3.1 = A1 rate limits OFF THE LEDGER (routes AND apps; 60s
   rolling window counted from invocations + an in-memory inflight term for
   burst exactness; 429 + Retry-After from the oldest in-window row; NO
   ledger row for a 429 — admission isn't a sandbox outcome; counter metric
   keel_fn_rate_limited_total) + A2 fn_logs (platform-api log captured in
   FnCtx, batch-written after the run with the invocation rowid; caps
   256 lines/invocation, 2048 B/line char-boundary, last-2000-per-ref trim;
   GET /api/logs with after= tailing; /logs drill-down UI page — NOT a nav
   tab; echo-fn now logs each body line) + A3 --retain-ledger-hours (0=off,
   immediate-first-pass 60s GC over invocations + fn_logs; fleet knob too).
   v3.2 = A4 CLI verbs deploy/bind/run/logs (thin clients of the HTTP API,
   ureq; --server/KEEL_SERVER, --token/KEEL_API_TOKEN same var as server).
   NO WIT CHANGE — zero guest rebuilds (echo-fn edit uses an existing
   import). New columns via CREATE TABLE line + ensure_column (cron
   precedent): routes.rate_limit, apps.rate_limit (NULL = unlimited).
   insert_invocation now RETURNS the rowid. New index idx_invocations_admit
   (kind, ref, created_at) + idx_fn_logs_ref (kind, ref, id).

   PROGRESS (tick after every task — this is the compact-resume pointer):
   - [x] O.1 schema: rate_limit columns + fn_logs + indexes + db accessors + unit tests
   - [x] O.2 admission (core function.rs): admit()/AdmitGuard + EngineShared inflight map + metric
   - [x] O.3 logs capture: FnCtx logs vec + truncate + batch write + trim; echo-fn logs lines
   - [x] O.4 surfaces: 429 in dispatch (fn + app api), /api/logs, api fields, /metrics counter
   - [x] O.5 --retain-ledger-hours GC + fleet knob
   - [x] O.6 UI: rate cols on /routes + /apps, /logs page + partial + row links
   - [x] O.7 accept_operate.sh ×2 from clean + angry review + docs + FULL suite
   - [x] O.8 ship v3.1: push, CI green (f8cf2f0), tag on CI SHA, release + 6 assets VERIFIED
   - [x] O.9 CLI: client.rs (deploy/bind/run/logs) + accept_cli.sh ×2 + angry review
   - [x] O.10 ship v3.2 same ritual; update memory + ROADMAP + demo instance
   - SHIPPED: v3.1 (tag on CI-green f8cf2f0) and v3.2 (tag on CI-green
     92caf8e), both releases with hand-written notes and 6 assets VERIFIED
     via the list endpoint. AMENDMENT 1 IS COMPLETE — suite is 20 gates /
     42 unit tests, all in CI. Further work is demand-driven only (ROADMAP
     Next: host-kv, replication, remaining stretch: wasi:http/proxy compat,
     wasi:keyvalue). The user's demo instance runs the v3.2 binary on
     demo.db (retrofitted in place): /fn/echo bound at rate 120 via
     `keel bind`, logs seeded, hello app + playground + usage all live.

   V3.2 RECORD (2026-07-19):
   - **client.rs**: Conn (clap-flattened --server/KEEL_SERVER +
     --token/KEEL_API_TOKEN — the server's own env var, one export = both
     roles) + one finish() choke point (2xx→JSON, non-2xx→"server said
     <code>: <body verbatim>" exit 1, transport→"cannot reach the engine").
     resolve_module: .wasm path uploads (name = file stem), else 64-hex
     passes through, else a helpful bail. deploy: recursive collect
     (dot-entries skipped at EVERY level, SYMLINKS SKIPPED — following them
     invites cycles/infinite recursion, angry-review catch), in-memory
     ZipWriter (SimpleFileOptions + Deflated), warning when no index.html,
     re-deploy = upsert end to end (gate-proven). run: watches with status
     transitions on stderr, output string on stdout, exit 0/1 by terminal
     state; --detach prints the id. logs: kind inferred from leading '/';
     --follow polls after=<last id> 1/s with explicit stdout flush.
   - **No engine-side code paths** (the A4 principle): everything the CLI
     does, curl can do; api.md says so. ureq parses JSON BY HAND
     (into_json sits behind a feature flag core doesn't enable — two lines
     beat feature drift; caught at first compile).
   - **Gate**: CLI PASS ×2 from clean + once more after the symlink fix —
     tokened engine; tokenless + wrong-token exit 1; --token AND env token
     both work; bind → tokenless curl through the public plane; run
     counter to completed (output + exit code + stderr transitions);
     deploy of a fabricated dir stores EXACTLY the 3 real files (.DS_Store
     stays home), css served with the right type, backend roundtrip live,
     re-deploy replaces index.html; logs for both inferred kinds.
   - clippy -D clean; CI step added (suite = 20 gates).

   V3.1 RECORD (2026-07-19):
   - **Admission (A1)**: core/function.rs admit() → Admission::{Admitted(AdmitGuard),
     Limited{retry_after_ms}}; ONE mutex around inflight-read + ledger COUNT +
     increment makes the limiter EXACT under bursts (gate: 8 concurrent at
     limit 3 → exactly 3×200/5×429); AdmitGuard releases on Drop so engine
     faults can't leak slots; key = "kind\0ref". 429 = admission, NOT a
     sandbox outcome → no ledger row, counter metric instead. Retry-After
     computed from the oldest in-window row (pure-inflight window → 1s).
     Window state = the ledger → the gate proves RESTART SURVIVAL (429
     immediately after reboot) and frees the window by BACKDATING rows
     instead of sleeping. Apps get the same check on api/* (kind 'app').
   - **Logs (A2)**: FnCtx collects (256 lines/invocation, 2048B/line
     char-boundary truncate, drop marker), batch-written AFTER
     insert_invocation (which now returns the rowid) inside invoke_handler;
     write failure is tracing::error, never a 500 (metering outranks
     observability). Per-ref trim to newest 2000 at insert. /api/logs
     (kind/ref/after/limit, after= = tail contract), /logs page + 2s partial
     (drill-down, NOT a nav tab; linked from /routes + /apps rows).
     echo-fn logs each body line (response unchanged — phase-4 safe).
   - **Retention (A3)**: --retain-ledger-hours, same immediate-first-pass
     60s loop as the workflow GC; sweeps invocations + fn_logs; fleet knob.
   - **Schema**: routes.rate_limit + apps.rate_limit via CREATE line +
     ensure_column (cron precedent; retrofit unit-tested); fn_logs +
     idx_fn_logs_ref + idx_invocations_admit. get_app now returns AppRec
     {backend_hash, rate_limit}; AppListRow 5-tuple.
   - **Angry-review catches**: (a) THE GATE HARNESS BUG — curl -w
     "%{http_code}" writes no newline, so cat rl-*.code concatenated into
     one line and grep -c read 0/0; the engine was EXACT all along (debug
     showed 200200200429429429429429). \n added + comment. (b) zsh
     unmatched-glob bit AGAIN in a debug chain (rm -f rl-*.code aborted the
     && chain → engine never started → misleading 000s) — bash + explicit
     filenames for debug harnesses. (c) i64::div_ceil is UNSTABLE (unsigned
     is stable) — manual (x+999)/1000. (d) apps.html json-enc script must
     parseInt rate_limit — a JSON string number is silently read as absent
     by as_i64 (comment in template). (e) fn_logs invocation_id can dangle
     after gc_ledger (created_at ms skew between the two inserts) —
     harmless, no FK, documented in the schema comment.
   - **Live probes** (beyond the gate): /logs 200 + renders ref, partial
     carries captured line, missing ref → 400, kind=solve → 400, form-shape
     bind with rate_limit round-trips, both pages render new columns.
   - **Suite**: OPERATE PASS ×2 from clean; FULL 19-gate suite green
     (chunks, idle machine); 42 unit tests (5 new: retrofit, window count,
     tail/after/trim, gc_ledger, truncate boundaries); clippy -D clean.

P. **The v3.2 audit + forward plan (2026-07-19).** Written on user request
   ("make a plan… angry reviewer for fixes, happy founder for ideas") after
   Amendment 1 shipped. Two voices, verified against the code — every fix
   below was CONFIRMED by reading it, not vibes. Nothing here is started;
   the user picks. (Verified-clean along the way: login cookie is already
   SameSite=Lax + digest-only — CSRF posture fine; zip caps, admission
   exactness, and journal invariants all hold.)

   ANGRY REVIEWER — THE FIX LIST (severity-ordered):
   - [x] P-FIX-1 (P0, info disclosure): PUBLIC-plane errors leak internals.
     dispatch.rs answers /fn/* and /apps/* engine faults with
     `{"error": format!("{e:#}")}` — full anyhow chains: module hashes, db
     paths, wasmtime compile errors, all to tokenless callers. Remedy:
     public plane gets a generic `{"error":"internal error"}` + a
     tracing::error with the chain; CONTROL plane keeps verbatim errors
     (operators are authed). Gate: probe asserts no hash/path in a
     provoked public 500.
   - [x] P-FIX-2 (P0, availability): NO global cap on concurrent function
     execution. Every /fn and /apps/api request parks one spawn_blocking
     thread for its whole run (up to time_limit_ms); the judge ALSO
     spawn_blockings per submission, uncapped, into the same default
     ~512-thread tokio pool. A flood of slow requests exhausts the pool —
     rate limits are per-ref and don't compose into a global bound.
     Remedy: `--max-fn-concurrent` (semaphore; over it → fast 503
     "at capacity" — honest backpressure beats queueing forever) + a judge
     concurrency of 1 with a bounded queue. Gate: flood a slow fixture,
     assert 503s + engine stays healthy + workflows unaffected.
   - [x] P-FIX-3 (P1, perf): every /fn request reads the FULL module BLOB
     from SQLite before checking the compiled cache — invoke_handler does
     get_module_wasm() then shared.component(hash, &wasm), which ignores
     the bytes on a hit. MB-scale copy per request for nothing. Remedy:
     component_cached(hash) fast path first; read bytes only on miss.
   - [x] P-FIX-4 (P1, memory): the compiled-component cache is UNBOUNDED
     (components: Mutex<HashMap>) — 500 uploaded modules = 500 JIT images
     held forever; first-compile also happens UNDER the cache lock
     (documented as accepted; re-evaluate with the cap). Remedy: small LRU
     (--max-compiled-modules, default ~64) — wasmtime Components are
     Arc-backed so eviction is safe while instances run.
   - [x] P-FIX-5 (P1, robustness): no read/response timeout on the data
     plane — a slow-drip 10 MiB body holds a connection + async task
     indefinitely (extract_raw's to_bytes has no deadline). Remedy: a
     timeout layer (~30s) on /fn + /apps routes only (control plane and UI
     polls untouched). Gate-able with a throttled upload.
   - [ ] P-FIX-6 (P2, asset perf, founder crossover): cache-control:
     no-store on EVERY asset forces re-downloading ~MB wasm on each app
     load. Trunk emits content-hashed filenames — those are immutable.
     Remedy: store sha256 per asset at upload; ETag + If-None-Match → 304
     everywhere, long max-age for hash-named files, keep no-store for
     index.html. Apps go from ~1 MB/load to ~1 KB.
   - [ ] P-FIX-7 (P2, CLI symmetry): the CLI can create but not inspect or
     undo — no `keel ls` (overview), `keel unbind <prefix>`,
     `keel apps rm`? (delete app needs an API route too — apps have no
     DELETE at all, only routes do!), `keel run --timeout` for scripts.
     The missing `DELETE /api/apps/{name}` (+ cascade assets) is a real
     API gap the CLI work would surface anyway.
   - [ ] P-FIX-8 (P2, polish): /favicon.ico 404s on every page view (log
     noise, browser retry). Embed a 1-file icon like htmx.min.js.
   - [ ] P-FIX-9 (P2, tests): admit() itself has no unit test — the
     inflight+ledger interplay is gate-covered only. Try an in-memory
     EngineShared (EngineOptions on :memory:); if construction is too
     heavy, extract the countable core into a testable fn and say so here.

   HAPPY FOUNDER — THE IDEA SHELF (leverage-ranked, each is a spec
   amendment or ROADMAP row before it is code):
   - P-IDEA-1 **wasi:http/proxy compatibility world** (SPEC-MICROCLOUD
     stretch): the dispatcher learns to drive unmodified ecosystem
     components — Spin apps, componentize-js/JCO output, anything
     targeting wasi:http. Today only keel-WIT guests run; this one change
     makes the ENTIRE component ecosystem deployable on a one-binary
     cloud. The single biggest adoption unlock on the board. (Amendment 2,
     phase-sized.)
   - P-IDEA-2 **wasi:keyvalue for functions** (stretch): a kv host
     interface backed by a table — functions get durable state without
     reaching for a workflow. Sessions, counters, caches: "real apps"
     territory. Pairs naturally with IDEA-1 (wasi guests expect it).
   - P-IDEA-3 **Function secrets/config**: workflows have secret(name);
     functions have NOTHING — they can't hold an API key, which walls
     them off from every external service. Per-route config set on the
     control plane, injected via a platform-api `config-get` (WIT bump)
     — the last thing between /fn and useful integrations.
   - P-IDEA-4 **`keel new <name> --template counter|static`**: scaffold
     apps/hello as a template (frontend + starter-fn + Trunk.toml with the
     public_url pitfall pre-solved). `keel new` → `trunk build` →
     `keel deploy` is a five-minute first-app story — DX is the moat for
     a dev-tools product.
   - P-IDEA-5 **Cron → functions**: schedules can only start workflows;
     let a schedule invoke a ROUTE (classic cron-job semantics, ledger
     rows included). Small, closes a whole use-case class.
   - P-IDEA-6 **Latency percentiles from the ledger**: duration_ms is
     already recorded per invocation — expose p50/p95/p99 per ref on
     /metrics and sparklines on /usage. Pure exposure of existing data;
     the observability story writes itself.
   - P-IDEA-7 **Usage export**: day-rolled CSV/JSON of the ledger per
     ref/module — the metering primitive any future hosted cloud bills
     from, useful standalone for chargeback today.
   - P-IDEA-8 **Public read-only playground mode**: problems + verdicts
     visible tokenless (submissions stay authed) — the judge is the most
     demo-able thing Keel has; let it demo itself.

   RECOMMENDED SEQUENCE (if the user says go): v3.3 "harden" =
   P-FIX-1..5 behind one gate (accept_harden.sh) — the two P0s are real
   and the three P1s are an afternoon each; v3.4 "polish" = P-FIX-6..9 +
   P-IDEA-6 (ETag + CLI symmetry incl. DELETE /api/apps + favicon +
   percentiles); then Amendment 2 for IDEA-1/2/3 (spec first, code
   second — same discipline as Amendment 1). Everything stays
   demand-driven: nothing above starts without the user's word.


Q. **v3.3 "harden" — P-FIX-1..5 built and shipped (2026-07-19).** The user's
   go-ahead ("continue with status.md unprompted") picked up §P's recommended
   sequence; v3.3 is its first slice. No spec amendment: no WIT change, no new
   API surface beyond serve flags — §P is the governing plan (hardening
   existing behavior, not extending the platform). All five fixes landed in
   one pass, gate-proven:
   - [x] Q.1 (P-FIX-3/4, core): ComponentCache — bounded LRU keyed by
     last-touch tick (linear eviction scan; cap default 64 via
     --max-compiled-modules / EngineOptions.max_compiled_modules).
     component_cached(hash, loader) is the new choke point: hash-first
     lookup, module BLOB read ONLY on a compile miss; first-compiles
     serialize on a dedicated compile_lock OUTSIDE the cache lock (a hit
     never waits behind a compile; N concurrent cold requests = ONE
     compile). component(hash, bytes) kept as a loader-closure wrapper —
     preflight/run_workflow/judge untouched. Accepted tradeoffs (documented
     in code): one extra byte-copy on cold workflow starts; cold compiles
     of DISTINCT modules serialize. wasmtime Components are Arc-backed, so
     evicting one with live instances is safe. keel_compiled_cache_size
     gauge. Unit tests: loader-skipped-on-hit, LRU eviction order (2 new,
     suite = 44).
   - [x] Q.2 (P-FIX-2, core): EngineShared.fn_sem (Arc<tokio Semaphore>,
     --max-fn-concurrent permits, default 64) + judge_sem (1 permit,
     hardcoded — identical budgets keep verdicts comparable) +
     fn_over_capacity counter. core gains tokio {features=["sync"]} ONLY
     (runtime-independent primitives; the core stays thread-based).
   - [x] Q.3 (P-FIX-2, engine): dispatch_fn try-acquires an OWNED permit in
     ASYNC context — after extract_raw (a dripping client must not hold an
     execution slot) and before spawn_blocking (an over-cap request never
     touches the pool; the permit also bounds the pool's queue depth from
     the data plane). serve_app takes a borrowed permit ONLY on the api/*
     branch — asset serving stays up under function saturation
     (gate-asserted). 503 {"error":"engine at capacity"} + Retry-After: 1 +
     keel_fn_over_capacity_total; 503s write NO ledger row. Judge:
     create_submission wraps its spawn_blocking in tokio::spawn { permit =
     judge_sem.acquire_owned().await; ... } — queued submissions cost no
     thread, 202-now preserved, permit held across the blocking join. If a
     timeout (P-FIX-5) drops the request future mid-run, the permit rides
     INSIDE the closure and frees only when the run actually ends — still
     correctly bounded.
   - [x] Q.4 (P-FIX-1, engine): dispatch.rs internal_error(context, e) —
     public plane answers {"error":"internal error"} constant; the full
     chain goes to tracing::error prefixed "public-plane" (greppable).
     All seven format!("{e:#}") sites replaced (run_function engine-fault,
     open_conn ×2, list_routes, admit ×2, get_app, get_asset, both join
     errors). Sandbox OUTCOME bodies ({"outcome":"tle"} etc.) and 404s
     naming caller-supplied refs stay — they describe the caller's own
     request, not engine internals. Verified: NO immutable gate assertion
     depended on the old leaky bodies (accept_phase5's public 500 assert is
     outcome-shaped).
   - [x] Q.5 (P-FIX-5, engine): tower-http TimeoutLayer
     (with_status_code 408 — ::new is deprecated and -D warnings would eat
     it) route-scoped on /fn/{*rest} + /apps/{*full} only;
     --data-timeout-secs default 30, clamped >= 1. Covers body upload AND
     execution — operators must keep it above their largest
     time_limit_ms (docs/operations.md). Control plane, UI polls, login:
     untouched.
   - [x] Q.6 fleet.rs: per-tenant max_fn_concurrent / max_compiled_modules
     / data_timeout_secs passthrough (a tenant's flood stays its own
     problem).
   - [x] Q.7 fixture guests/burn-fn (world handler): black_box busy-loop
     (the phase-5 LLVM lesson) that a quota must end — bound with huge fuel
     + time_limit_ms 2000, every request deterministically holds a slot
     ~2s with outcome tle. Committed like echo-fn (Cargo.lock +
     bindings.rs in).
   - [x] Q.8 gate scripts/accept_harden.sh (#21, in CI after accept_cli):
     compile-fail module (\0asm + junk passes the upload door check,
     fails Component::new — chosen because wrong-WORLD modules fail at
     instantiate, which classify() records as a trap OUTCOME, not an
     engine fault) → 500 body EXACTLY {"error":"internal error"}, no
     hash/wasm/compil substrings, "public-plane" in engine.log, 0 ledger
     rows; saturation anatomy probe (2 background burns → 503 +
     Retry-After: 1 + body) while /apps/h/ assets still 200; burst 6 at
     cap 2 → EXACTLY 2×500(tle)/4×503, keel_fn_over_capacity_total == 5
     (1 anatomy + 4 burst), burn ledger rows == 4 all tle, /fn/e 200
     after (permits free); 2 back-to-back submissions both AC (judge
     queue drains); keel_compiled_cache_size == 2 exactly after 3
     distinct modules (echo, burn, sum — deterministic: bad never
     compiles, both submissions share sum's hash) + evicted module
     re-serves 200; python3 slow-drip socket → 408 at
     --data-timeout-secs 4 (4 > the ~2.1s burn ceiling — margin for slow
     CI), happy path 200 after. Engine caps in-gate: fn 2 / cache 2 /
     timeout 4.
   - [x] Q.9 docs: operations.md 3 flag rows + bounded-public-plane
     paragraph + fleet optional keys; ROADMAP v3.3 row; status.md
     cheatsheet → 21 scripts (also backfilled the accept_cli.sh line v3.2
     forgot); .gitignore accept-hard debris (the rl-*.body lesson).
   - [x] Q.10 suite: accept_harden ×2 from clean + ALL 21 gates green in
     foreground chunks on an idle machine + 44 unit tests + clippy
     --release -D warnings clean.
   Angry-review findings: the instantiate-vs-compile probe correction
   (caught at design time, above); everything else accepted-with-comment
   (copy on cold start, serialized cold compiles). No code defects
   survived to commit.

   V3.3 SHIPPED (2026-07-19): commit d36bf8ece1be448ccaee95ce4a14366f213a8e03
   pushed → CI green (both jobs, all 21 gates) → release v3.3 created with
   --target on that exact SHA + hand-written notes → release.yml success →
   6 assets (linux x86_64/arm64 + macOS arm64 tarballs, each with .sha256)
   verified via the releases LIST endpoint. Suite at ship: 21 gates /
   44 unit tests / clippy --release -D warnings. The user's demo engine on
   :8080 was killed for the suite runs; restart on the new binary if
   wanted (nohup target/release/keel serve --db demo.db).

R. **Next steps after v3.3 — the refined plan (2026-07-19, on user request).**
   §P stands as written (append-only; its P-FIX-1..5 are ticked and §Q is
   their record). This section refines the REMAINING shelf with what the
   v3.3 build taught.

   STATUS: APPROVED 2026-07-19 — the user's word: "do 3.4 and 3.5 and
   beyond". The sequence below is live, built in order with the full
   discipline per stage (angry review → gate ×2 from clean → whole suite →
   ship: CI-verified tag, hand-written notes, 6 assets verified). v3.4
   IN BUILD; v3.5 starts with SPEC-AMENDMENT-2.md; v4.0 starts with
   SPEC-AMENDMENT-3.md.

   v3.4 "polish" — P-FIX-6..9 + P-IDEA-6, one gate (accept_polish.sh),
   no WIT change, additive schema only:
   - [x] R.1 (P-FIX-6) asset caching: store sha256 per asset at upload
     (assets table gains an `etag` column via ensure_column — the
     schedules.cron retrofit precedent); serve_asset sends ETag and
     honors If-None-Match → 304 empty body; Cache-Control splits:
     content-hashed filenames (trunk's `-<hex>.` pattern) get
     `max-age=31536000, immutable`, index.html KEEPS no-store (it is the
     upgrade lever — never cache the entry point). Gate: GET carries
     ETag → conditional GET 304s → re-upload changes the ETag → 200
     again → index.html stays no-store. Apps go ~1 MB/load → ~1 KB.
   - [x] R.2 (P-FIX-7) CLI symmetry + the missing API: FIRST the API
     hole — DELETE /api/apps/{name} (app row + assets cascaded in ONE
     txn; 404 honest; routes already have DELETE, apps have none).
     Then the verbs, thin clients per A4: `keel ls` (routes + apps +
     schedules, one screen), `keel unbind <prefix>`, `keel apps rm
     <name>`, and `keel run --timeout <secs>` (exit 2 on deadline —
     scripts need a bound; today a wedged workflow wedges the CLI).
   - [x] R.3 (P-FIX-8) favicon: one embedded file served like
     htmx.min.js (build.rs-asserted), route allowlisted in auth like
     /assets/*. Kills a 404 per page view.
   - [x] R.4 (P-FIX-9) admit() unit tests — UNBLOCKED by v3.3: §P hedged
     "if EngineShared construction is too heavy, extract the core"; the
     v3.3 runner.rs tests settled it (test_shared builds a REAL
     EngineShared on a scratch db, cheaply). Reuse that helper: seed
     invocations rows at the window edge, assert recent+inflight vs
     limit boundaries, AdmitGuard Drop releases the slot, Limited's
     retry_after_ms derives from the oldest in-window row (None → 1000).
   - [x] R.5 (P-IDEA-6) latency percentiles: p50/p95/p99 per ref from
     invocations.duration_ms — small N per ref, compute in SQL
     (ORDER BY duration_ms + LIMIT/OFFSET index math; no dependency,
     no histogram buckets to mis-size) — exposed as
     keel_fn_duration_ms{ref="...",quantile="..."} gauges; /usage gains
     three columns. Metrics first; sparklines only if asked.
   - Optional while in there (only if trivial): keel_judge_queue depth
     gauge — SKIPPED (not trivial enough to be free: the semaphore has no
     waiter count without extra state; revisit on demand).

   V3.4 RECORD (2026-07-19): all five R-items landed in one pass. Deltas
   from the plan, discovered while building: (a) `keel ls` surfaced a
   SECOND API hole — apps had no GET either; GET /api/apps added alongside
   DELETE (both documented in docs/api.md). (b) hash_named() also treats a
   BARE 12+-hex stem as content-named (a file literally named by its hash)
   — comment matches code. (c) favicon is a real committed 16×16 ICO
   (generated once by hand-rolled struct-packed BMP-in-ICO; SVG probing is
   browser-dependent, ICO isn't), served at /favicon.ico, auth-allowlisted,
   build.rs-asserted like htmx. (d) If-None-Match matching is a contains-
   check on the 64-hex etag (quote/W\-prefix agnostic; can't false-match);
   `If-None-Match: *` deliberately not special-cased (a PUT guard, not a
   GET reality). (e) admit() tests seed rows through the REAL
   insert_invocation and manipulate the window with direct UPDATEs — the
   operate-gate technique at unit level; runner::testutil::shared is the
   cfg(test) helper both test mods share. Suite = 22 gates / 51 unit tests
   (7 new: 4 admit, percentiles nearest-rank, etag-moves-with-bytes,
   delete-app cascade). Gate accept_polish.sh runs on a TOKENED engine so
   the favicon-allowlist claim is real; run --timeout proven exit-2 against
   a parked approval workflow that KEEPS RUNNING after the CLI stops
   watching (asserted via the API).

   v3.5 "functions grow up" — SPEC-AMENDMENT-2.md FIRST (the Amendment 1
   in-repo discipline), then code. ONE WIT bump (0.7.0 → 0.8.0 — the
   one-bump-per-stage rule is why config and kv ship TOGETHER, and why
   they ship BEFORE the wasi work): platform-api gains
   - config-get(name) -> option<string> (P-IDEA-3): per-route/per-app
     config set on the control plane, values stored in the db and NEVER
     echoed by list endpoints (names only — the secrets redaction
     posture); the API-key unlock, i.e. the last thing between /fn and
     real external integrations.
   - kv-get(key) -> option<list<u8>> / kv-set(key, value) (P-IDEA-2's
     platform-api half): durable per-ref state in a new fn_kv table
     keyed by the same (kind, ref) identity the ledger and fn_logs use;
     hard caps per ref (key count + total bytes) so a public listener
     cannot grow the db — the fn_logs bounding precedent. Sessions,
     counters, caches — real apps without reaching for a workflow.
   Additive WIT: existing guests rebuild only to ADOPT the imports.
   Gate accept_functions2.sh: config visible to the guest / absent →
   none / values never in list responses or logs; kv survives restart;
   caps enforced with honest errors; app backends get both.

   V3.5 RECORD (2026-07-19): BUILT AND SHIPPED per SPEC-AMENDMENT-2.md
   (authored first, in-repo — A6 config, A7 kv, A8 the 0.8.0 rebuild
   contract, A9 acceptance). Deltas and decisions:
   - The amendment ADDED kv-delete beyond the plan's get/set — a store you
     can't delete from fills its caps forever; and it names the atomicity
     line out loud: per-call only, no CAS (kv-cas listed as future; atomic
     sequences are what workflows are for).
   - fn_config + fn_kv tables keyed by the SAME (kind, ref) identity as
     ledger/logs/limits; unbinding cascades NEITHER (config = operator
     intent, kv = guest state); explicit removal via DELETE /api/config
     and DELETE /api/kv (the "reset my function" button). Retention flags
     deliberately do not touch kv (state, not history).
   - FnCtx gained kind/refname; host impls: config-get degrades db errors
     to `none` (the guest surface has no error case, the operator log gets
     the truth); kv-set door-checks key ≤256 B / value ≤64 KiB then runs
     the cap check + upsert under ONE IMMEDIATE txn (angry-review catch:
     check-then-insert would over-admit the per-ref caps of 1024 keys /
     8 MiB under concurrency; commit is the durability point, still before
     the call returns).
   - Control plane: POST/GET/DELETE /api/config (names-only listing —
     values NEVER echoed), GET/DELETE /api/kv (keys-only; wipe). Token-
     gated, /api/logs param style.
   - Fixture guests/kvcfg-fn (world handler): /cfg /count /reset /big —
     each new call observable through plain HTTP.
   - accept_functions2.sh (gate 23, in CI): ref-scoped config with a leak
     check on the listing + door-check 400s; counter 1,2,3 → kill -9 → 4
     (A7 durability); the SAME module at a second prefix counts from 1
     and an app backend counts from 1 (ref-scoped, not module-scoped,
     across kinds); config survives restart; /reset; the over-cap err
     names 65536; keys-only /api/kv then wipe → 1. Ran 3× from clean
     (incl. post-txn-fix).
   - A8 proven the strong way: the ENTIRE 23-gate suite re-ran green under
     0.8.0 — every guest rebuilt in its own gate, zero source changes
     outside the new fixture. Solver world untouched (import-free binaries
     keep judging). Unit tests 53 (kv roundtrip+caps via the real Host
     impl, config ref-scoping incl. cross-kind).
   - One clippy catch: the api.rs insert initially split upload_assets
     from its doc comment (empty-line-after-doc-comment) — moved intact.

   v4.0 "the ecosystem release" — SPEC-AMENDMENT-3.md: wasi:http/proxy
   compatibility (P-IDEA-1) + wasi:keyvalue (P-IDEA-2's ecosystem half).
   Phase-sized, and the spec must settle THE dependency decision up
   front: adopt wasmtime-wasi + wasmtime-wasi-http for the proxy world's
   host surface (they are the reference implementation; hand-rolling
   wasi:io/streams is a losing game) — a deliberate new dependency
   family, taken in a spec, not mid-code. Dispatcher inspects the
   uploaded component's exports and drives world handler OR
   wasi:http/proxy per route; wasi:keyvalue backs onto v3.5's fn_kv
   table (same caps, same identity). Result: unmodified Spin /
   componentize-js / JCO output deploys on a one-binary cloud — the
   single biggest adoption unlock on the board.

   V4.0 GROUNDWORK (2026-07-19, derisked before the host build):
   SPEC-AMENDMENT-3.md AUTHORED (E1 dependency decision, E2 lazy world
   detection at INVOKE — bind stays existence-only because accept_harden
   pins 201-then-500-at-request for junk modules, E3 same-walls
   invocation, E4 outbound = per-ref operator grant default-deny, E5
   wasi:keyvalue on fn_kv default-bucket-only, E6 vendored wits, E7
   gate). PROVEN ALREADY: (a) wasmtime-wasi-http 43.0.2 HAS the sync
   path — p2::add_to_linker_sync + p2::bindings::sync — so ONE engine
   stands; (b) guests/proxy-echo BUILDS: pure wasi:http/proxy@0.2.6 + a
   wasi:keyvalue/store@0.2.0-draft import, wits VENDORED from the
   wasmtime-wasi-http/-keyvalue 43.0.2 crate sources. TWO TOOLING
   LESSONS: cargo-component 0.21.1 does NOT auto-scan wit/deps for local
   path targets — every dep package needs an explicit
   [package.metadata.component.target.dependencies] entry; and its older
   wit-parser applies @unstable feature gates asymmetrically (parses the
   gated interface OUT of clocks but still resolves cli's gated import
   of it) — the vendored copies have @unstable-gated IMPORT pairs
   stripped (host unaffected; wasmtime skips them). println! is a NO-OP
   on wasm32-unknown-unknown — the fixture writes wasi:cli stdout
   streams explicitly. Remaining build: core deps + sync proxy runner
   (invoke_proxy beside invoke_handler, same Quota/ledger/admission),
   export-surface detection cached by hash, keyvalue host bindgen! on
   fn_kv (hand-rolled Host impl — 5 fns on a table we own), outbound
   stub-vs-real linker by routes.allow_outbound (ensure_column),
   guests/proxy-out, accept_ecosystem.sh, ci step.

   V4.0 RECORD (2026-07-19): BUILT AND SHIPPED — the ecosystem release.
   core/src/proxy.rs is the whole runner: sync wasi-http embedding on the
   ONE engine (E1 held — no second engine), ProxyCtx = ResourceTable +
   WasiCtx (stdout/stderr = MemoryOutputPipe 64 KiB, read back into the A2
   log pipeline via function::capture_lines) + WasiHttpCtx + KeelHooks +
   MemLimiter + conn + (kind, ref). KeelHooks::send_request is the E4
   gate: deny = HostFutureIncomingResponse::ready(Err(HttpRequestDenied))
   — DATA to the guest, never a trap; grant = default_send_request;
   made-count → keel_fn_outbound_total. wasi:keyvalue = hand-rolled Host
   on fn_kv via bindgen! over wit-wasi-keyvalue/ (kv_set_bounded is the
   ONE shared cap wall with platform-api kv); default bucket only.
   THE SYNC-EMBEDDING SHAPE (hard-won): the response body channel buffers
   ONE chunk, so a collector task runs CONCURRENTLY on
   wasmtime_wasi::runtime (spawn before the call, in_tokio+timeout(5s)
   await after into_data drops the table/writers) — and at the RESP_CAP
   the collector DRAINS WITHOUT STORING instead of dropping the receiver
   (dropping traps the still-writing guest and masks the verdict; drain
   is bounded by the guest's own quotas). wasi:io contract lesson:
   blocking-write-and-flush takes AT MOST 4096 B/call — bigger buffers
   trap BY DESIGN (the /big fixture chunks accordingly). Detection (E2):
   world_of walks the compiled export surface ("handle" vs
   "wasi:http/incoming-handler@0.2"), cached in
   EngineShared.guest_worlds; run_function detects THEN dispatches to
   invoke_handler or invoke_proxy, and both worlds' responses unify into
   function::RawResponse for one translation. Misbound wrong-world
   modules now surface as engine faults (generic 500, no ledger row)
   instead of trap outcomes — verified unpinned by any gate. routes/apps
   gained allow_outbound (ensure_column, JSON-only field, echoed by GET).
   Accepted costs (documented): one extra conn open per request for the
   detection loader; proxy guests see authority()=None (Host header still
   forwarded); collector await bounded at 5s post-return. Gate
   accept_ecosystem.sh (#24, in CI): pure-wasi roundtrip + ledger row +
   stdout line in /api/logs + keel-world route beside it; wasi:keyvalue
   counter 1,2,3 → kill -9 → 4 + /api/kv listing; outbound denied
   in-band then granted → "upstream 200: pong" + metric == 1; starvation
   fuel → oof on the wire and in the ledger; 11 MiB response →
   guest_error with the engine NOT wedged (next request counts). Ran ×2
   from clean + the FULL 24-gate suite green + clippy -D warnings + 53
   unit tests. NO keel-WIT change — v3.x guests did not rebuild.

   SHIP CHAIN, all three approved stages (2026-07-19): v3.4 tag on
   c44e31c2172f, v3.5 tag on 121a93f33256, v4.0 tag on 14cb8d1a734e —
   each: CI green on the exact SHA → gh release create --target →
   release.yml success → 6 assets (linux x86_64/arm64 + macOS arm64 +
   sha256s) verified via the releases LIST endpoint. §R is fully
   executed. THE SHELF NOW: P-IDEA-4 (`keel new` templates), -5
   (cron→functions), -7 (usage export), -8 (public playground), kv-cas
   (named in Amendment 2), provider host-kv, native replication (cloud-
   gated), authority()=Some for proxy guests (accepted-cost follow-up).
   All demand-driven — nothing starts without the user's word.

   Why this order (founder voice): v3.4 is a day of polish that makes
   every demo feel professional (304s, favicon, `keel ls`) and closes
   the audit; v3.5 is SMALL, pure-keel, completes the "real apps" story
   (state + secrets), and builds machinery v4.0 reuses; v4.0 is the
   adoption swing and deserves a fresh phase with its dependency
   decision made deliberately. The rest of the shelf (P-IDEA-4
   templates, -5 cron→functions, -7 usage export, -8 public playground)
   stays ranked behind these three; the hosted cloud stays
   adoption-gated (VISION.md).

S. **v4.1 audit + hardening — the whole v3.3→v4.0 diff re-reviewed, angry
   (2026-07-19, unprompted continuation "review all changes like an angry
   reviewer… note ideas like a happy founder").** A multi-agent adversarial
   pass over the 92caf8e..HEAD diff (6 review dimensions × per-finding
   independent verification): 15 candidate findings, **13 CONFIRMED against
   the actual code + the vendored wasmtime-wasi-http source, 2 REFUTED as
   intended design**. Every fix below was applied surgically (match the
   file's own conventions) and the tree kept green: **cargo build + clippy
   --release -D warnings clean, 54 unit tests (was 53; +1 negative-cache), and
   the FOUR gates covering every changed area re-run PASS from clean —
   accept_harden (concurrency/cache/timeout + the negative-cache path),
   accept_functions2 (config atomic cap + kv), accept_polish (percentiles/
   ETag/run--timeout/apps GET+DELETE), accept_ecosystem (proxy outbound +
   wasi:keyvalue)** — PLUS a NEW gate **accept_hardv41.sh PASS** proving the
   fixes no prior gate reached: S-FIX-1 (the P1 — a granted outbound to a
   HANGING upstream is bounded to ~time_ms and the permit frees, single AND
   under a cap-saturating flood), S-FIX-5 (a quote in a `ref` yields escaped
   /metrics), S-FIX-8 (the config cap holds under an 8-way concurrent burst).
   This is an audit slice, NOT a tagged release: the FULL 25-gate run + gate
   coverage for the last two fixes (S-FIX-3 forged-header bomb needs a crafted
   zip fixture; S-FIX-10 --timeout 0 is CLI-loop logic) must land before any
   v4.1 tag (see S.3).

   ANGRY REVIEWER — CONFIRMED + FIXED (severity-ordered):
   - [x] S-FIX-1 (P1, availability — the headline): PROXY OUTBOUND HAS NO
     HOST WALL-CLOCK BOUND. `KeelHooks::send_request` (proxy.rs) forwarded the
     guest's `OutgoingRequestConfig` into `default_send_request` unclamped;
     wasmtime-wasi-http's per-phase timeouts default to 600s AND the guest can
     raise them. The proxy runs SYNC on the spawn_blocking thread, so a guest
     parked in an outbound holds its `fn_sem` permit while NO wasm executes —
     neither the epoch trap nor fuel can fire (both need a wasm boundary), and
     the /fn TimeoutLayer 408s the client WITHOUT freeing the detached
     closure's permit. N concurrent granted-outbound requests to a slow/hung
     upstream pin all `--max-fn-concurrent` permits → the whole data plane
     503s. This is exactly the P-FIX-2 invariant ("a permit is held only up
     to time_limit_ms") violated. FIX (taken ALL THE WAY, gate-proven by
     accept_hardv41.sh): a PROVABLE O(time_ms) bound on the permit hold from
     three parts — (a) clamp connect/first-byte/between-bytes each to the
     route's `time_ms` (clamp DOWN only), so a single silent hang can't burn
     wasi-http's 600s default; (b) refuse a NEW outbound once the invocation's
     wall-clock `budget` is spent, so a guest can't chain sub-budget calls; and
     (c) the store's epoch_deadline (ceil(time_ms/100) ticks, bumped every
     100ms of WALL time by the runner ticker) traps the guest at its NEXT wasm
     boundary once wall-clock exceeds time_ms — and between any two host calls
     the guest MUST run wasm to issue the next, so that boundary always
     arrives. The "drip-feed residual" the first pass flagged is thus CLOSED,
     not deferred: each per-frame gap is ≤ budget (a), and the drip loop's
     inter-frame wasm lets the epoch (c) trap it — worst case one in-flight
     phase (≤ budget) plus the trap. A separate thread-watchdog was
     considered and rejected (Simplicity First): it would guard only against
     an error in THIS bound argument, at real complexity/risk in a mature
     tree; the argument holds because every unbounded host wait in a proxy
     guest is one of the three clamped wasi-http phases (kv → local SQLite
     busy_timeout; stdout → non-blocking pipe). The provider path
     (provider.rs) already passed an explicit timeout; the proxy path lacked
     it. Gate: a granted outbound to a HANGING upstream returns in ~time_ms
     (not 600s/the curl cap) and a fast route serves right after; a flood of
     hung outbounds at the cap clears in ~time_ms and the data plane stays
     healthy.
   - [x] S-FIX-2 (P2, availability): UNCACHED COMPILE FAILURES convoy every
     `fn_sem` permit on the global `compile_lock`. `component_cached`
     (runner.rs) never recorded a `Component::new` failure, so a bound-but-
     broken module (bind is existence-only — a supported state) re-paid a full
     BLOB copy + failed compile under `compile_lock` on EVERY request; a
     tokenless flood of 64 parks all permits in the lock queue and stalls cold
     workflow spawns/upgrade-preflight too. FIX: a NEGATIVE cache (hash →
     error, bounded) checked before the compile path; content-addressed, so a
     stale entry can never shadow a fixed module (a fix changes the hash).
     Unit-tested (component_cache_negative_caches_compile_failures).
   - [x] S-FIX-3 (P2, availability): ZIP-BOMB CAP BYPASSABLE. The app-asset
     upload pre-checked `entry.size()` (the attacker-forgeable HEADER claim,
     forgeable to 0) then did an UNBOUNDED `read_to_end` — a ~60 MiB zip of
     zeros (deflate ~1032:1) materialised tens of GiB before the post-check,
     OOM-killing the engine (auth-gated → P2, but the advertised defense did
     not work). FIX: bounded read — `take(remaining+1)` then reject if over —
     at most ~MAX_UNPACKED is ever allocated regardless of the header.
   - [x] S-FIX-4 (P3×2, poison-tolerance): several liveness-critical locks
     used bare `.lock().unwrap()` against the file's OWN documented convention
     (Permit::drop et al. use `unwrap_or_else(PoisonError::into_inner)` because
     "a release path must never panic"). `compile_lock` + the components-cache
     sites (runner.rs) and `fn_inflight` in `admit()` + `AdmitGuard::drop`
     (function.rs): one panic in a critical section poisons the lock; the next
     `unwrap` panics, and a panic in `AdmitGuard::drop` DURING unwind aborts
     the whole process (every workflow thread). FIX: poison-tolerant locking
     at all those sites (safe — `compile_lock` guards no data, the cache holds
     only good components, the map is a u32 counter). Near-theoretical trigger
     (rusqlite/wasmtime surface errors as Results) but catastrophic-if-fired.
   - [x] S-FIX-5 (P3×2, observability availability): `/metrics` interpolated
     route prefixes into Prometheus label VALUES unescaped. A route prefix
     legally may contain `"` or `\` (create_route only checks `/fn/` + no
     trailing slash + no `..`; the http crate accepts both raw in a path);
     one such ref in a `ref="…"` label makes the WHOLE scrape unparseable →
     every keel metric goes dark until the ledger rows age out (deleting the
     route doesn't clear them). FIX: `escape_label()` (backslash/quote/newline)
     applied at emission — covers all current and future refs.
   - [x] S-FIX-6 (P3, correctness under GC): `duration_percentiles` ran its
     GROUP-BY COUNT and each `OFFSET` query in SEPARATE WAL snapshots; the
     ledger GC deleting rows in between made a stale-`n` OFFSET fall past the
     shrunken set → QueryReturnedNoRows → a 500'd /metrics scrape (and /usage).
     FIX: wrap the whole read in one `unchecked_transaction` (single snapshot).
   - [x] S-FIX-7 (P3, perf): `duration_percentiles`' per-ref `ORDER BY
     duration_ms` had NO covering index (only idx_invocations_admit on
     created_at), so it temp-b-tree-sorted the ref's whole ledger 3× per scrape
     AND per /usage load — O(refs·N log N), unbounded under the keep-forever
     default retention. The doc comment even claimed an index that didn't
     exist. FIX: additive `idx_invocations_latency (kind, ref, duration_ms)`
     (CREATE IF NOT EXISTS — retrofits old DBs at startup).
   - [x] S-FIX-8 (P3, invariant): `fn_config`'s 64-entry cap was check-then-act
     on autocommit — N concurrent authed POSTs push a ref to 64+(N-1). The
     project ALREADY fixed this exact class for fn_kv (kv_set_bounded's
     IMMEDIATE txn, a documented v3.5 angry-review catch); config missed it.
     FIX: mirror it — check + upsert under one IMMEDIATE txn.
   - [x] S-FIX-9 (P3, audit blind spot): `GET /api/apps` (and `keel ls`)
     OMITTED `allow_outbound` — the v4.0 outbound grant was invisible to every
     read path (echoed only at grant time), while GET /api/routes includes it.
     A security inventory of outbound-capable refs silently missed apps. FIX:
     `list_apps` SELECTs + emits it (AppListRow gained the field).
   - [x] S-FIX-10 (P3, CLI correctness): `keel run --timeout` checked the
     deadline BEFORE polling, so a workflow completing in the last sleep window
     was reported with a STALE "still running" status, and `--timeout 0` exited
     2 with a fabricated "starting" and ZERO polls. FIX: reorder to
     poll → terminal-match → deadline-check → sleep (≥1 poll guaranteed; exit-2
     status is now an observed at-or-after-deadline fact).
   - [x] S-FIX-11 (P3, CLI robustness): `keel deploy` did `fs::read` on any
     non-dir non-symlink entry — a FIFO in the deploy dir blocks `open(2)`
     forever with no output (silent hang). FIX: only read regular files; skip
     FIFO/socket/device with a warning like the symlink case.

   REFUTED (intended design — recorded so they're not re-litigated):
   - Proxy `allow_outbound` does no egress/destination filtering (SSRF once
     granted). REAL behavior, but it IS the SPEC-AMENDMENT-3 E4 design (E4
     names the SSRF threat and prescribes exactly the default-deny +
     token-gated per-ref grant; no allowlist specified) — same trust model as
     workflow http-request and effectful providers. A destination allowlist
     across all THREE outbound paths is a hardening enhancement, demand-driven.
   - No cross-validation between route `time_limit_ms` and the data-plane
     TimeoutLayer (mismatched routes 408 while the sandbox keeps a permit).
     Intended: a guest BURNING wasm is bounded by fuel/epoch, so the permit
     frees when the run ends within time_ms — the operator invariant "keep
     --data-timeout-secs above your largest time_limit_ms" is documented. (Note
     the contrast with S-FIX-1, which is the genuinely-unbounded case: a guest
     parked in a HOST outbound where fuel/epoch can't fire.)

   HAPPY FOUNDER — THE EXPANDED IDEA SHELF (three tracks; all compose with
   the existing primitives and each other; nothing repeats P-IDEA-4/5/7/8,
   kv-cas, provider host-kv, replication). Each is a spec-amendment/ROADMAP
   row before it is code — the Amendment-1 discipline.

   PLATFORM DEPTH — turn the primitives into a closed compute mesh:
   - M-1 **`invoke-function` from workflows** (host-api, journaled like
     http-request; idempotency key wfid:seq): functions already call workflows
     (start-workflow/get-workflow); this closes the missing edge so every
     deployed fn is a durable activity library for every workflow. Reuses
     invoke_handler + admit + the same fn_sem permit + one honest global cap.
     Small. THE edge that makes "compute fabric" true.
   - M-2 **Durable topics** — ONE pub-sub primitive, three delivery modes
     (broadcast = per-sub cursor at-least-once; queue = lease+DLQ competing
     consumers; wake = bridge into host.rs park loops). Producers: workflows
     (journaled emit → replay never double-publishes), functions, control
     plane (webhook ingestion for free), cron. Delivery loop = the scheduler/GC
     pattern; ledger row per delivery. Phase-sized, but SUBSUMES P-IDEA-5
     (cron→fn becomes "schedule targets a topic"). Highest-compounding item.
   - M-3 **Schedules become a fabric peer** — target union {workflow | topic |
     fn ref}; with M-1/M-2 a DAG is just a workflow that invoke-functions and
     emits (the durable core already IS the DAG engine — journaled edges,
     crash-proof resume). Don't build a YAML DSL. An afternoon after M-1/M-2.
   - M-4 **Named wasi:keyvalue buckets with grants** — `open("<name>")`
     resolves through a bucket_grants table (the allow_outbound pattern:
     default-deny, in-band failure, a metric). One state fabric, many
     consumers; the wasi:keyvalue semantics Spin guests already expect. Small.
   - M-5 **Registry federation** — providers/modules are already sha256 blobs;
     `keel add <url>/<name>@sha256:<h>` fetches from any HTTPS index, verifies
     the hash, upserts through the existing preflight. A "marketplace" is a
     signed static JSON index — no central service, in keeping with the SQLite
     positioning. The Envoy-filters analogy gets its distribution channel.
   - M-6 **Cell bridges** — a subscription target may be a REMOTE keel;
     delivery uses journaled http-request + idempotency keys → at-least-once
     cross-cell with dedupe on the receiver's topic_events. Multi-region as
     FEDERATED keels, not a cluster — the invariant (one writer/node) intact.
   Smallest set that makes the fabric real: M-1+M-2+M-3 (one WIT bump,
   Amendment 4 = invoke-function + emit; topics/queues/targets are host-side).

   ECOSYSTEM & ADOPTION — why developers show up and stay (all CLI/DX, no WIT
   bump; only E-5 touches schema):
   - E-1 **`keel dev`** — watch a source dir, rebuild (cargo-component/trunk/
     jco), POST the module, rebind-by-hash (the v2.6 precedent → live swap into
     the v3.3 LRU), stream logs. The inner loop. Pure client.rs + a watcher.
   - E-2 **`keel check <wasm>`** — run v4.0's world_of + import introspection
     at the CLI: report detected world, needed grants (--allow-outbound, kv,
     config), and print the exact `keel bind`. Converts every wrong-world/
     missing-grant 500 into a pre-deploy sentence. Closes the E2 gap without
     touching bind semantics.
   - E-3 **`keel import spin <spin.toml>`** — v4.0 already runs unmodified Spin
     components; parse the MANIFEST (sources→upload, triggers→routes,
     allowed_outbound_hosts→grant, [variables]→config, key_value_stores→already
     fn_kv). "Point Keel at your Spin app and it runs on one binary." A
     wadm.yaml reader is the same shape later.
   - E-4 **`npm create keel-app`** — componentize-js/JCO already runs (v4.0
     proved it incl. wasi:keyvalue + stdout→fn_logs); ship a JS template with
     the vendored-wit lessons pre-solved. Aims P-IDEA-4 at the ~20:1-larger JS
     population.
   - E-5 **`keel push`/`keel pull`** — named tokenless-READ module bindings
     (P-IDEA-8 posture); every keel IS a registry. Integrity is free (the hash
     is the key). keel-to-keel first, OCI/warg only on demand.
   - E-6 **`keel-deploy` GitHub Action** (XS) + E-7 **`/inspect/<ref>`** — one
     page correlating an invocation's logs + verdict + fuel + peak-mem +
     percentiles (all already keyed by (kind,ref)); the observability demo that
     sells itself.

   MOONSHOTS (honest about reality level):
   - QUANTUM-POWERED WASM, decomposed: **M1a quantum-as-an-effectful-provider
     is REAL NOW and fits the grain perfectly** — a `quantum` provider
     (keel:provider@0.2.0, --provider-effectful) wrapping Braket/IBM/D-Wave
     REST; the WORKFLOW does the durable submit→sleep→poll loop. Quantum cloud
     jobs queue minutes-to-hours and cost real money per shot — EXACTLY the
     crash-proof-long-running-job shape durable workflows exist for.
     Journal-before-return + idempotency keys (v2.3) mean submit fires exactly
     once, replay returns the recorded job-id, hybrid QAOA/VQE loops are just a
     workflow with a provider call in the body. ~1–2 days against one vendor,
     ZERO engine/WIT change, ships next to providers/relay. The same shape
     works for ANY expensive async job API (GPU training, batch renders) — it
     pays for itself as a pattern even if quantum advantage never lands.
     **M1b QRNG→random** (real, niche/marketing). **M1c PQC** — honest: the
     token layer is a static bearer, no asymmetric crypto to break; where PQC
     actually bites is MODULE SIGNING (ML-DSA over the content hash, verified
     at bind — the supply-chain story, and the substrate M3 needs). **M1d
     quantum-solver-for-the-judge = say no** — the solver world's identity is
     ZERO imports; a quantum backend inverts that. And for the record: nothing
     runs WASM *on* a quantum computer and nothing will; Keel's honest claim is
     quantum-*orchestrating*, which is better because it's true.
   - M2 **THE AGENT RUNTIME — every AI agent is a durable workflow** (the
     biggest swing). The machinery is ~90% built: journaled http-request +
     idempotency = crash mid-tool-loop resumes WITHOUT re-paying tokens
     (deterministic agents free, from invariant #1); the approval fixture IS
     human-in-the-loop; cancel/epoch is the kill-switch; --wf-fuel-limit is the
     runaway guard; kv versioning is agent memory that survives live upgrade;
     live v1→v2 upgrade is swapping an agent's prompt/model MID-FLIGHT; secrets
     holds the key; ledger + rate limits are per-agent cost control; cells are
     per-customer isolation. New code: providers/llm (effectful, ~days) + a
     `keel new agent` template. "Durable agents in one 8 MB binary on your own
     box, agent state in one SQLite file you can read" — nobody has that.
   - M3 **Verifiable execution** — (1) signed modules (M1c), (2) journal
     head-chaining (one ensure_column: each row hashes the previous → a
     tamper-evident execution receipt), (3) TEE cells. The journal is ALREADY
     an audit log; Keel can make it a CRYPTOGRAPHIC one cheaply BECAUSE there's
     one writer and one file — no distributed-consistency hole. Verdicts become
     credentials (upgrades P-IDEA-8's playground into proctoring).
   - M4 **The time-machine debugger** — `keel debug <wf-id>`: recovery already
     IS re-execution against the recorded journal (invariant #2 did the hard
     part years early), so the debugger is recovery + breakpoints. Killer verb:
     `--fork --module <new-hash>` replays the journal prefix against a CANDIDATE
     module and shows where its effect sequence diverges — turns upgrade
     pre-flight from yes/no into a diff. Composes with M2 (step an agent's
     recorded decision trace).
   - M5 **Mid-flight workflow migration** — `keel migrate <id> --to <server>`:
     quiesce at a journal boundary, export rows + (content-addressed) module,
     resume on the target = the existing recovery scan on a different box.
     Single-writer never violated (a migrated_to tombstone; no shared-write
     moment). Dissolves VISION's one conceded limitation into a feature; the
     spine a hosted cloud needs anyway.
   FOUNDER RANKING: M2 agents is the adoption swing (~2 wks, no WIT bump);
   M1a quantum provider is days for an outsized narrative + a real small market
   (do it as the flagship effectful-provider demo); M4 debugger is the best
   pure-product moonshot; M3 layers 1–2 are cheap differentiation; M5 waits for
   fleet demand.

   HEXAGONAL / PORTS & ADAPTERS — swappable components (added on user request).
   STATUS: STARTED 2026-07-19 (user: "commit and start hexagonal"). Governing
   doc [SPEC-AMENDMENT-4.md](SPEC-AMENDMENT-4.md) (spec-first discipline). H-1
   (name the ports) is written there; the FIRST port — **secret-store** — is
   BUILT in-tree (core/src/secrets.rs): the `SecretStore` trait + `FileSecretStore`
   (the default, refactored verbatim from `lookup_secret`) + `EnvSecretStore`
   (the second adapter that earns the port — `--secrets-env-prefix`, 12-factor/k8s)
   + `LayeredSecretStore` (file-before-env) + `NoSecretStore`. `secret()` in
   host.rs now calls `self.secrets.get()` — all journaling/salted-hash/redaction
   is UNTOUCHED above the port. Green on build/clippy/58 unit tests (+4 secrets)
   and smoke_secrets.sh grew an env-adapter leg (same guarantees, no file). The
   remaining ports (Store, outbound, blob, clock) stay concrete until their
   second adapter is demanded (H-3 guardrail).
   The founder framing: Keel ALREADY won the hard half of hexagonal
   architecture, and nobody says so out loud. The WASM capability boundary IS a
   ports-and-adapters boundary — a guest (the application core) has ZERO ambient
   capability and reaches the outside world ONLY through host-defined ports (the
   WIT interfaces host-api / platform-api); the journal sits ON that port, and
   the capability PROVIDERS (v2.2–v2.6: content-addressed, hot-swappable,
   tier-graded `keel:provider` components) are already first-class, dependency-
   inverted ADAPTERS plugged into it. That is the differentiator vs every
   native-plugin engine, and it's shipped. The expansion is to turn the SAME
   pattern INWARD — the engine's own infrastructure dependencies become named
   DRIVEN PORTS with swappable adapters, so "one binary, one SQLite file" is the
   DEFAULT adapter set, not a hardcoded fate. The INVARIANTS become the port
   CONTRACTS every adapter must honor — which is exactly how a cell grows
   without betraying the vision.
   - H-1 **Name the ports.** Driven ports (engine depends on): durable-Store,
     outbound-http, secret-store, blob-store, clock, event-sink. Driving ports
     (drive the engine): the HTTP API, the CLI, the embedded lib (v2.2 Engine
     façade — already a driving port!), cron, and M-2 topics. Mostly a
     naming/refactor pass that makes the rest tractable.
   - H-2 **Swappable Store (the big one).** db.rs hardcodes rusqlite/SQLite;
     define a `Store` trait carrying the journal/ledger/kv ops whose CONTRACT is
     "one writer, journal-commit-before-return." SQLite is one adapter (the
     default); a Postgres adapter lets a cloud cell outgrow one file WITHOUT
     becoming a cluster (still one-writer-per-workflow via advisory lock); a
     libSQL/Turso adapter gets embedded-replica replication for free (closes the
     "native replication" ROADMAP row without reimplementing WAL streaming);
     :memory: (tests already use it) becomes an explicit adapter. This is the
     scale story that keeps the invariant.
   - H-3 **Swappable outbound-http adapter** — unifies the THREE outbound paths
     (workflow http-request in host.rs, effectful providers, proxy KeelHooks)
     behind ONE port. Adapters: the default (ureq / wasmtime-wasi-http), a
     deterministic mock (record-replay = VCR for testing agents/workflows), a
     circuit-breaker, and THE egress allowlist the refuted SSRF finding wants —
     write it ONCE as an adapter, get it on all three paths.
   - H-4 **Swappable secret-store** — `secret(name)` is the driving port,
     --secrets-file just one adapter; Vault / AWS Secrets Manager / SOPS / env
     plug in WITHOUT touching the journal's salted-hash + redaction discipline
     (the adapter yields bytes; the security posture stays engine-side).
   - H-5 **Swappable blob-store** for module/asset bytes — content-addressed, so
     the hash is the key and S3/filesystem/OCI are just "where the bytes live";
     composes with M-5 registry federation.
   - H-6 **Testable-by-swap** — every adapter gets a fake, so the engine core is
     testable without SQLite/network/wasmtime, and the "embeddable" positioning
     gets stronger (embedders assemble Keel from adapters for their stack).
   ANGRY-REVIEWER GUARDRAIL on this idea (so it doesn't become a trait
   explosion — Simplicity First): extract a port ONLY when a SECOND real adapter
   is demanded (a Postgres cell, a Vault shop). Until then SQLite / ureq / file
   stay concrete. Define the port at the moment of the second adapter, never
   speculatively — the same demand-driven discipline as the rest of the shelf.
   The provider system is the existence proof that the model works; H-2..H-5
   are that proof turned toward the engine's own dependencies.

   S.3 — DONE THIS PASS: (a) S-FIX-1 taken all the way — the residual is
   CLOSED by the budget-denial + clamp + epoch bound (no watchdog needed; see
   the S-FIX-1 entry) and gate-proven; (b) accept_hardv41.sh WRITTEN + PASSING
   (scripts/hang_stub.py is its hanging upstream), covering S-FIX-1 / S-FIX-5 /
   S-FIX-8.
   STILL BEFORE A v4.1 TAG: (c) gate coverage for S-FIX-3 (needs a crafted
   forged-uncompressed-size zip fixture — python zipfile writes honest headers,
   so this wants hand-packed bytes) and S-FIX-10 (`keel run --timeout 0` polls
   once — CLI-loop assertion; low-risk, the reorder is self-evidently correct);
   (d) the FULL 25-gate suite on an idle machine (accept_hardv41 joins CI after
   accept_ecosystem); (e) docs/api.md GET /api/apps `allow_outbound` row +
   operations.md proxy-outbound timeout note; (f) then the ship ritual
   (CI-verified tag, hand-written notes, 6 assets). Until then this is an
   in-tree hardening slice, green on build / clippy -D warnings / 54 unit tests
   / accept_harden + functions2 + polish + ecosystem + hardv41.

---

## What exists (file map, current through v4.0)

```
keel/
├── Cargo.toml               workspace = ["core", "engine"], exclude guests+providers (dev. 5)
├── SPEC.md                  the base build spec — source of truth for phases 1–3
├── SPEC-MICROCLOUD.md       micro-cloud extension (phases 4–6), pristine external copy
├── SPEC-AMENDMENT-1.md      v3.1/v3.2: rate limits off the ledger, fn logs, retention, CLI
├── SPEC-AMENDMENT-2.md      v3.5: per-ref config + durable kv (WIT 0.8.0 — a rebuild event)
├── SPEC-AMENDMENT-3.md      v4.0: wasi:http/proxy compat + wasi:keyvalue + outbound grants
├── SPEC-AMENDMENT-4.md      the hexagonal ports & adapters program (§S H-track); first
│                            port = secret-store (file + env adapters), in-tree
├── VISION.md / ROADMAP.md / PROVIDERS.md / README.md
├── docs/                    api.md (full endpoint table), operations.md (flags/DR/fleet/
│                            upgrade notes), guests.md, deploy/ (systemd, docker, k8s)
├── wit/workflow.wit         keel:workflow@0.8.0 — host-api (workflows), platform-api
│                            (functions: log/now/random/start-workflow/get-workflow/
│                            config-get/kv-*), worlds workflow + handler + solver
├── provider-wit/            keel:provider 0.2.0 — pure + effectful worlds
├── wit-wasi-keyvalue/       host-side bindgen target for keel's wasi:keyvalue impl (v4.0);
│                            deps/keyvalue.wit vendored, same file the proxy fixtures use
├── core/  (keel-core — everything journal-shaped + the sandboxes)
│   └── src: lib.rs (Engine façade + EngineOptions), db.rs (schema/migrate/ALL SQL),
│        journal.rs (§6 journaled() — the heart), host.rs (workflow host-api + park loops),
│        runner.rs (EngineShared, component LRU cache, thread-per-workflow, permits,
│        testutil), function.rs (handler sandbox + invoke_handler + admit + kv_set_bounded
│        + capture_lines), proxy.rs (v4.0: sync wasi-http runner, world detection,
│        wasi:keyvalue Host, outbound hooks), judge.rs (solver sandbox), sandbox.rs
│        (Quota/MemLimiter/classify), provider.rs, cron.rs, notifier.rs,
│        secrets.rs (Amendment 4: the secret-store PORT + file/env/layered adapters)
│        + tests/embedded.rs, examples/embedded.rs
├── engine/  (the `keel` binary)
│   ├── build.rs             asserts vendored htmx.min.js + favicon.ico exist
│   ├── assets/              htmx.min.js, style.css, favicon.ico (all embedded)
│   ├── templates/           askama pages + polling partials (dashboard, workflows, modules,
│   │                        schedules, providers, routes, playground, usage, apps, logs, login)
│   └── src: main.rs (CLI: serve/backup/fleet + client verbs; router; GC/backup/scheduler
│        loops; data-plane TimeoutLayer), api.rs (the whole control plane incl. config/kv,
│        metrics), dispatch.rs (PUBLIC data plane: /fn + /apps, world-aware run_function,
│        admission, permits, asset ETags), auth.rs, ui.rs, client.rs (thin CLI verbs),
│        fleet.rs (cell supervisor)
├── guests/                  fixtures, each standalone (Cargo.lock + generated bindings.rs
│   │                        committed; target/ ignored)
│   ├── workflow world:      demo, approval, counter (v1/v2), spin, effects, secrets,
│   │                        kvup, kvup2, loadgen, providerdemo, relay
│   ├── handler world:       echo-fn, starter-fn, burn-fn (v3.3 slow fixture),
│   │                        kvcfg-fn (v3.5 config+kv fixture)
│   ├── solver world:        sum-solver, loop-solver, hog-solver
│   └── wasi:http/proxy:     proxy-echo (+wasi:keyvalue counter, /big cap probe),
│                            proxy-out (outbound fixture) — wits VENDORED under each
│                            (cargo-component needs explicit target.dependencies entries)
├── providers/               greet (pure), relay (effectful) sample providers
├── apps/hello/              Leptos 0.7 CSR demo app (trunk; public_url="./" REQUIRED)
├── .github/workflows/       ci.yml (clippy -D warnings + unit tests + all 24 gates),
│                            release.yml (v* tag → 3 platform tarballs + sha256s, by release ID)
└── scripts/                 24 acceptance/smoke gates in CI + accept_hardv41.sh (§S, the
                             audit-fix gate — passes locally, joins CI at v4.1 tag time) +
                             hang_stub.py (its hanging upstream) + stub/ (offline http)
```

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
   (The 2026-07-16 hardening changed the scripts' HARNESS — cleanup, readiness,
   local stub — never their assertions. That distinction is the line.)
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

Post-review hardening verification (2026-07-16):

- `cargo clippy --release -p keel-engine --all-targets -- -D warnings`: clean.
- `cargo test --release -p keel-engine`: **8/8 pass** (journaled() ×4, event
  delivery, finish_sleep, upgrade txn incl. un-delivery, finish_cancel).
- `scripts/smoke_cancel.sh`: **CANCEL SMOKE PASS** — a sleeping counter AND a
  pure-wasm spinner both cancelled (200 → failed/"cancelled by operator"); the
  spinner proves the epoch-deadline → AbortForUpgrade → silent-exit chain works
  end to end. Junk upload → 400; re-cancel → 409; timers table empty after.
- All three phase gates re-run green with every hardening change in place
  (phase 1 fully offline via the stub): `PHASE 1 PASS`, `PHASE 2 PASS`,
  `PHASE 3 PASS`, sequentially, fresh DBs.

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
- **The regression suite is: `cargo test -p keel-engine`, then all three phase
  scripts, then `smoke_cancel.sh`.** Run them after any engine change; CI runs
  the same set on every push to main. They need port 8080 free (and 18080 for
  phase 1's stub) and `cargo-component` on PATH (~/.cargo/bin) — no public
  internet.
- **Extending the WIT**: adding an import is source-compatible but old uploaded
  .wasm BLOBS keyed to an older interface version will not instantiate (see the
  WIT-versioning caveat above). Adding/renaming exports is breaking for all
  guests. Journal `kind`/payload shapes (§4.2) are ON-DISK format — never rename.
- **Known accepted limitations** (spec non-goals — do not "fix" casually):
  at-least-once effects on crash between exec and journal INSERT; parked
  workflows hold --max-running permits; a mid-sleep upgrade restarts the sleep
  in full; no auth/TLS/clustering/metrics; guest HTTP is GET-only, capped at
  1 MiB with silent truncation. (Runaway guests are NO LONGER unstoppable —
  epoch interruption + the cancel endpoint landed in the 2026-07-16 hardening.
  A guest blocked in a long HOST call still can't be interrupted mid-call;
  cancel answers 409 "busy executing", retry after the call returns.)
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
- Kill a stray engine: `pkill -f 'keel serve'` (since the 2026-07-16 hardening
  the scripts trap-clean their engine (and phase 1 its stub) on EVERY exit,
  success or failure — a stray keel means someone ran it by hand).
- A workflow you just want GONE: `POST /api/workflows/<id>/cancel` — works on
  parked AND spinning guests; 409 if it is mid-host-call (retry) or terminal.

---
