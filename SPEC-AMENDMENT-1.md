# Amendment 1 to the micro-cloud extension — operating the public plane + the CLI

Status: AUTHORED IN-REPO (2026-07-19), per SPEC-MICROCLOUD.md "Stretch directions":
*"per-route rate limiting off the ledger ... Each of these is a spec amendment
first, code second — same discipline as everything above."* This document is that
amendment. SPEC-MICROCLOUD.md itself stays a pristine copy of the external spec
(rev 1.0); everything here is additive to it.

Ships as two stages: **v3.1** (A1–A3: rate limits, function logs, ledger
retention) and **v3.2** (A4: the client CLI). Acceptance scripts
`scripts/accept_operate.sh` and `scripts/accept_cli.sh` are the definition of
done — same immutability rules as every gate before them (SPEC.md §0).

**No WIT change. Zero guest rebuilds.** Everything in this amendment is
host-side (A1–A3) or client-side (A4). `keel:workflow` stays 0.7.0. The one
guest edit (echo-fn gains `log` calls, A2) uses an import `world handler` has
had since 0.7.0.

Motivation: v3.0 made the data plane (`/fn/*`, `/apps/*`) deliberately PUBLIC
(status.md §N.5). A public plane you cannot rate-limit, whose ledger grows
without bound, and whose functions log into the void is a demo, not a platform.
And a platform you can only drive with hand-rolled `curl | python3` one-liners
is a platform with no front door. This amendment closes those four gaps and
nothing else.

---

## A1. Per-route (and per-app) rate limits — OFF THE LEDGER

The stretch direction's phrase is load-bearing: the invocations ledger — already
written for EVERY sandbox outcome — is the rate-limit state. No token buckets,
no new counters to persist, no drift between "what we metered" and "what we
enforced". Consequences the builder must preserve:

- **Restart-safe for free.** The window is rows in a table; an engine restart
  changes nothing about what is admitted next.
- **Observable for free.** `SELECT COUNT(*)` shows exactly what the limiter
  sees; the acceptance script manipulates `created_at` instead of sleeping.
- **429s are NOT ledger rows.** The ledger records *sandbox outcomes* (spec
  Task 4.3: engine faults excluded — deliberate). An admission rejection never
  reaches a sandbox. Rejections are visible as a Prometheus counter instead:
  `keel_fn_rate_limited_total`.

Definition: `rate_limit` = max **admitted** invocations per rolling 60 000 ms
window, per route prefix (kind `function`) or per app name (kind `app`).
NULL/absent = unlimited (the default; existing databases keep behaving as
before — `ensure_column` retrofit, the schedules.cron precedent).

Admission check, run where the route/app row is already in hand, BEFORE the
sandbox spins up:

```
recent   = COUNT(invocations WHERE kind=? AND ref=? AND created_at > now-60000)
inflight = in-memory per-(kind,ref) count of admitted-but-not-yet-ledgered runs
admit iff recent + inflight < rate_limit
```

The `inflight` term exists because ledger rows are written AFTER the run
(inside `invoke_handler`): without it, a concurrent burst of N requests all
reads the same `recent` and the limit oversubscribes by the concurrency. With
it the limiter is exact: the gate fires 8 concurrent requests at a limit of 3
and asserts **exactly** 3× 2xx and 5× 429. Check-and-increment must be atomic
(one mutex around both); the decrement is a Drop guard so an engine fault
can't leak a slot. New index `idx_invocations_admit ON invocations(kind, ref,
created_at)` keeps the COUNT off a table scan.

Rejection: HTTP 429, body
`{"error":"rate limited","limit":N,"window_ms":60000,"retry_after_ms":M}`,
header `Retry-After: <ceil(M/1000)>s`, where M is computed from the oldest
in-window row (the moment a slot actually frees), not a flat 60.

Surface: `rate_limit` accepted by `POST /api/routes` (JSON + form) and
`POST /api/apps`; echoed by `GET /api/routes`; shown on the `/routes` and
`/apps` pages (`∞` when unset).

## A2. Function logs — `platform-api.log` gets a destination

`log` today goes to engine tracing only — invisible to whoever deployed the
function. New table:

```sql
CREATE TABLE IF NOT EXISTS fn_logs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    kind          TEXT NOT NULL,          -- 'function' | 'app'
    ref           TEXT NOT NULL,          -- route prefix or app name
    invocation_id INTEGER,                -- ledger rowid; ties lines to a run
    line          TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_fn_logs_ref ON fn_logs (kind, ref, id);
```

Capture rules (the guest is untrusted; every cap is host-side):

- Lines collect in the invocation's `FnCtx`; they are written in ONE batch
  after the run, tagged with the invocation's ledger rowid
  (`insert_invocation` now returns it) — so a line is always attributable to
  the run that produced it, and log writes can never slow the guest mid-run.
- **256 lines per invocation**; beyond that, lines are counted, dropped, and
  a single final marker line records how many.
- **2048 bytes per line**, truncated on a char boundary (never mid-UTF-8).
- **Last 2000 rows per (kind, ref)** retained, trimmed at insert time — a
  chatty function under load cannot grow the table without bound between GC
  passes. Time-based retention is A3's job.
- Solvers cannot log: `world solver` imports nothing (phase 5's empty-linker
  sandbox is the feature, not a gap). Workflow logging is untouched — the
  journal is its surface.

Read side: `GET /api/logs?kind=function|app&ref=<...>&after=<id>&limit=<n>`
(control plane, token-gated; limit clamps to 1..=1000, default 100). Without
`after`: the newest `limit` rows, oldest-first. With `after`: rows with
`id > after`, oldest-first — the tail-following contract used by the UI's
polling partial and the CLI's `--follow`.

UI: a `/logs?kind=&ref=` drill-down page (2s polling partial, like a workflow
page — NOT a nav tab), linked from every `/routes` row and `/apps` row.

echo-fn gains one behavior: it logs `echo: <line>` for each line of its
request body (response unchanged — phase-4 assertions hold). That makes it
both the logs fixture and a live demo of the pipeline.

## A3. Ledger retention — `--retain-ledger-hours`

Same contract as `--retain-terminal-hours` (v1.3): default 0 = keep forever;
when > 0, a GC loop (immediate first pass, then every 60s) deletes
`invocations` AND `fn_logs` rows with `created_at` older than the window.
Fleet tenants get the matching `retain_ledger_hours` knob. Rate limiting reads
a 60-second window, so any sane retention setting (hours) cannot interact
with A1.

## A4. The client CLI — the curl-free platform (v3.2)

Four verbs, all thin clients of the existing HTTP API (no engine-side code
paths — the API stays the single control plane; anything the CLI can do, curl
can do). Connection flags on every verb: `--server URL` (default
`http://127.0.0.1:8080`, env `KEEL_SERVER`) and `--token` (env
`KEEL_API_TOKEN` — the same variable the server reads; one exported var makes
a shell both a server and a client).

- **`keel deploy <dir> --name <app> [--backend <file.wasm>] [--rate N]`** —
  the flagship: directory → running app in one command. Zips `<dir>` in
  memory (dot-prefixed entries skipped — .DS_Store is not a frontend), uploads
  the backend module first if given, upserts the app, uploads the bundle,
  prints the app URL. Re-running re-deploys (upsert semantics end to end).
- **`keel bind <prefix> <file.wasm> [--fuel N] [--mem-mb N] [--time-ms N]
  [--rate N] [--name <module-name>]`** — upload module + bind route, one step.
- **`keel run <file.wasm|module-hash> [--input JSON] [--detach]`** — start a
  durable workflow and (by default) watch it: poll until terminal, print
  status + output; exit 0 on `completed`, 1 on `failed`. A `.wasm` path is
  uploaded first; anything else is treated as a module hash. `--detach`
  prints the workflow id and exits.
- **`keel logs <ref> [--kind function|app] [--follow]`** — tail `fn_logs`.
  Kind is inferred when omitted: a ref starting with `/` is a route prefix,
  otherwise an app name. `--follow` polls `after=<last id>` once per second.

Errors: any non-2xx response prints the status and body verbatim and exits 1
(the server's error messages are already written for humans; do not re-word
them client-side).

## A5. Acceptance

**`scripts/accept_operate.sh` (v3.1)** must prove, in order: echo-fn logs
arrive with correct content and `after=` tail semantics; a limit-3 route
under 8 concurrent requests admits exactly 3 (5× 429 with Retry-After +
`keel_fn_rate_limited_total` 5, ledger rows exactly 3); a limit-1 app 429s
its second backend call; **a restart preserves the window** (the same route
429s immediately after reboot — the off-the-ledger property, asserted); a
backdated window frees the route (no sleeps — UPDATE `created_at`, the
ledger IS the state); `--retain-ledger-hours` sweeps backdated `invocations`
and `fn_logs` rows on the restart's immediate GC pass.

**`scripts/accept_cli.sh` (v3.2)** must drive the platform end to end through
the CLI alone (no direct curl except to verify results): `keel bind` then a
request through the bound route; `keel run` on the counter guest watching to
`completed` with the right output and exit code; `keel deploy` of a fabricated
static+backend app dir, then the app serves under `/apps/<name>/` with correct
content types; `keel logs` returns the lines the bound function logged; a bad
token exits 1 against a tokened server, and the same commands succeed with the
token — both via `KEEL_API_TOKEN` and `--token`.

Both scripts follow the house rules: offline, self-cleaning, fresh DBs,
`:8080` guard, foreground engine kills by PID, `sqlite3 -cmd ".timeout 5000"`
for any query that can race a write.
