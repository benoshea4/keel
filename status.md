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

---

## What exists (file map, all phases)

```
keel/
├── Cargo.toml               workspace = ["core", "engine"], exclude guests+providers (dev. 5)
├── core/                    keel-core LIBRARY (v2.2 split): db/journal/notifier/host/
│                            runner/cron/provider + the Engine façade (lib.rs), the
│                            embedded gate (tests/embedded.rs, examples/embedded.rs)
├── provider-wit/            keel:provider worlds — pure + effectful (PROVIDERS.md)
├── providers/greet/         sample PURE provider component (smoke_providers.sh)
├── providers/relay/         sample EFFECTFUL provider (smoke_providers_effectful.sh, v2.5)
├── SPEC.md                  the build spec — THE source of truth
├── status.md                this file
├── README.md                quick start + cancel/tests/security sections (the spec's verbatim
│                            runaway-guest warning was retired by hardening E — it is no longer true)
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
│       ├── api.rs           JSON API, 7 endpoints incl. upgrade + cancel; several also accept form/multipart
│       └── ui.rs            askama-render-to-Html handlers + embedded assets + upgrade control
├── guests/demo/             Task 1.6 acceptance guest (src/bindings.rs is GENERATED — don't edit);
│                            fetch url read from input {"url": ...} (hardening A)
├── guests/approval/         Task 2.9 acceptance guest: await-event("approve") → sleep 60s → output
├── guests/counter/          Task 3.7 acceptance guest: v1/v2 via feature flag; ticks + checkpoints
├── guests/spin/             cancel-me fixture: spins in pure wasm forever (hardening E)
├── .github/workflows/ci.yml clippy -D warnings + unit tests + all four scripts (hardening F)
├── .github/workflows/release.yml  v* tag → stripped binaries (linux x86_64, macOS arm64)
│                            tarred + sha256, attached to the GitHub release
└── scripts/
    ├── accept_phase1.sh     Task 1.7 assertions; harness hardened (trap/readiness/local stub)
    ├── accept_phase2.sh     Task 2.10 — kill -9 at both park points; W1==W2; UI smoke
    ├── accept_phase3.sh     Task 3.8 — pruning; resume recovery; v1→v2 live upgrade; 409 negative
    ├── smoke_cancel.sh      cancel both ways: parked (park loop) + spinning (epoch trap)
    ├── smoke_auth.sh        v1.1 gate: bearer/cookie auth + guest memory cap
    └── stub/body.txt        fixed body served on :18080 by accept_phase1.sh
```
(v1.1 additions inside engine/: src/auth.rs — token middleware; templates/login.html.)

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
