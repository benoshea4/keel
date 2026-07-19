# Keel roadmap

Positioning and the reasoning behind this sequence live in [VISION.md](VISION.md).
Every shipped item below is gated by a script under `scripts/` (they are the
definition of done) and runs in CI.

## Shipped

| Version | What | Proof |
|---|---|---|
| v3.4 | **Polish** (status.md §R.1–R.5): conditional GETs on app assets (per-asset sha256 `ETag`, `If-None-Match` → 304, hash-named files `max-age=1y immutable`, `index.html` stays `no-store` — app loads go ~1 MB → ~1 KB); API symmetry `GET /api/apps` + `DELETE /api/apps/{name}` (cascades assets, one txn); CLI symmetry `keel ls` / `unbind` / `apps rm` / `run --timeout` (exit 2, workflow keeps running); embedded `/favicon.ico` (auth-allowlisted); latency percentiles p50/p95/p99 per ref from the ledger's `duration_ms` (`keel_fn_duration_ms` in /metrics + a Latency-by-ref table on /usage); admit() unit tests (the §P P-FIX-9 hedge closed) | `accept_polish.sh` |
| v3.3 | **Hardening the public plane** (status.md §P, the v3.2 audit's P0/P1 fixes): engine faults answer a generic `{"error":"internal error"}` publicly with the full chain in the engine log (tokenless callers get no hashes/paths/compiler output); `--max-fn-concurrent` global sandbox-execution cap (503 + `Retry-After` beyond it, `keel_fn_over_capacity_total`, asset serving takes no slot, judge runs serialize on a 1-permit queue instead of stacking blocking threads); hash-first compiled-component lookup (the module BLOB is read only on a compile miss) behind a bounded LRU (`--max-compiled-modules`, `keel_compiled_cache_size`, compile outside the cache lock); `--data-timeout-secs` whole-request deadline on `/fn` + `/apps` (408 kills slow-drip bodies). No WIT change, no API change beyond flags | `accept_harden.sh` |
| v3.2 | **The client CLI** ([SPEC-AMENDMENT-1.md](SPEC-AMENDMENT-1.md) A4, the curl-free platform): `keel deploy <dir>` (directory → running app: in-memory zip, dot-files/symlinks skipped, backend upload, upsert end to end), `keel bind <prefix> <wasm>` (upload + bind with all quotas incl. `--rate`), `keel run <wasm\|hash>` (durable workflow watched to a terminal state; exit 0/1), `keel logs <ref> [--follow]` (kind inferred). Thin clients of the HTTP API — `--server`/`KEEL_SERVER`, `--token`/`KEEL_API_TOKEN` (the server's own variable) | `accept_cli.sh` |
| v3.1 | **Operating the public plane** ([SPEC-AMENDMENT-1.md](SPEC-AMENDMENT-1.md) A1–A3, the stretch item "per-route rate limits off the ledger" done with the discipline it asked for): per-route AND per-app `rate_limit` (max admitted runs per rolling 60 s, counted off the invocations ledger + an in-flight term — EXACT under concurrent bursts, restart-safe by construction; 429 + honest `Retry-After`; `keel_fn_rate_limited_total`); captured function logs (platform-api `log` → `fn_logs`, 256/invocation · 2 KiB/line · newest 2000/ref, `GET /api/logs` with `after=` tailing, `/logs` drill-down page); `--retain-ledger-hours` GC. No WIT change | `accept_operate.sh` |
| v1.0 | Durable core: journal-before-return, replay recovery, durable timers, external events, checkpoints + pruning, live v1→v2 upgrade with pre-flight, cancel (parked + spinning guests via epoch interruption), htmx UI | `accept_phase{1,2,3}.sh`, `smoke_cancel.sh`, `cargo test` |
| v1.1 | Operator token auth (`--api-token`/`KEEL_API_TOKEN`, cookie-digest UI login), per-guest memory caps (`--max-guest-memory-mb`) | `smoke_auth.sh` |
| v1.2 | Effects: `http-request` (method/headers/body, non-2xx as data, opt-in retries), journaled per-workflow KV (`kv-set`/`kv-get`), interval schedules (`/api/schedules`) — WIT 0.4.0 | `smoke_effects.sh` |
| v1.3 | Operability: `GET /api/workflows` (paged, filtered), `/metrics` (Prometheus text), retention GC (`--retain-terminal-hours`), UI logout | `smoke_effects.sh` |
| v2 (slice) | **DR**: periodic online snapshots (`--backup-dir`/`--backup-interval-secs`/`--backup-keep`) + one-shot `keel backup`, restore-by-copy runbook. **Cell tenancy**: `keel fleet --config` — one process/db/token per tenant, supervised, crash-only restarts | `smoke_dr.sh`, `smoke_fleet.sh` |
| v2.1 | **Secrets**: `secret(name)` host call (WIT 0.5.0) — `--secrets-file`, journal stores name + salted sha256 only, replay verifies against the live file (rotation fails loudly), values redacted from journaled HTTP requests. **Cron schedules**: 6-field expressions (UTC, seconds resolution) + `PATCH /api/schedules/{id}` pause/resume. **Per-call `timeout-ms`** on http-request | `smoke_secrets.sh`, extended `smoke_effects.sh` |
| v2.2 | **Crate split**: `keel-core` library (open/recover, upload, start, inspect — the binary is one consumer); "embeddable" is now literal. **Capability providers** (WIT 0.6.0, [PROVIDERS.md](PROVIDERS.md)): import-free `keel:provider` components registered via `--provider name=path.wasm` (per-tenant in fleets), journaled as `custom:<name>:<kind>`, memory+CPU bounded, failures as data | `smoke_embedded.sh`, `smoke_providers.sh`, extended `smoke_fleet.sh` |
| v2.3 | **KV versioning**: append-only `(workflow_id, key, seq, value)`; reads take the highest version, upgrades discard tail versions with the journal tail (the documented kv-vs-upgrade caveat is CLOSED), checkpoints compact superseded versions. **Idempotency keys**: `keel-idempotency-key: <workflow_id>:<seq>` on every http-request — wire-only (old journals replay untouched), stable across replay/re-send, guest-overridable, opt-out via empty value. No WIT bump | `smoke_kv_upgrade.sh`, extended `smoke_effects.sh` |
| v2.4 | **Surface & scale polish**: schedules page (create/pause/resume/delete) + durable-KV section on the workflow page · linux-arm64 release binaries · OTel traces behind `--features otel` (span per workflow, child span per host call, OTLP/http; default binary unaffected) · `keel_active_permits` metric · deploy recipes (`docs/deploy/`: systemd, Docker/compose, single-replica k8s) | `load_test.sh` (200 workflows through a cap of 8: cap respected via the permit gauge, all complete, journals dense), extended `smoke_effects.sh` |
| v3.0 | **Micro-cloud phase 6 — hosted full-stack apps. THE PLATFORM RELEASE**: upload a zip (Rust→WASM Leptos frontend + any static assets) and bind a backend function — Keel serves the app (`/apps/<name>/`, assets from SQLite with correct content types, SPA fallback), the app calls its backend (`./api/*` → the phase-4 dispatch core), the backend starts durable workflows. Browser-served WASM UI → function → durable workflow → result, ONE binary, ONE SQLite file. Zip-slip and zip-bomb uploads rejected all-or-nothing; `/apps` UI for create/upload/open | `accept_phase6.sh` |
| v2.8 | **Micro-cloud phase 5 — sandbox metering + the playground judge**: per-invocation `MemLimiter` (peak recorded, over-cap growth denied → MLE, never misread as RE), a LeetCode-style judge for `world solver` components (ZERO imports — the tightest sandbox in the platform; per-case 10⁹ fuel / 256 MiB / 2000 ms; AC·WA·TLE·MLE·OOF·RE, first non-AC stops), `/playground` UI with verdict badges + fuel/peak-mem columns, `/usage` ledger page (totals by module + last 100). The workflow runaway watchdog proven under a starvation `--wf-fuel-limit` | `accept_phase5.sh` |
| v2.7 | **Micro-cloud phase 4 — serverless functions** (WIT 0.7.0 adds `world handler` + `interface platform-api`; the [micro-cloud extension spec](SPEC-MICROCLOUD.md) lands as v2.7 → v2.8 → v3.0): bind a component to a `/fn/<name>` prefix (`POST /api/routes`, per-route fuel/mem/time quotas, longest-prefix dispatch, 10 MiB body cap, `/routes` UI), fresh sandboxed instance per request, a usage-ledger row for EVERY invocation; functions start and query durable workflows via `start-workflow`/`get-workflow` — Lambda + Step Functions in one process. Engine-wide fuel + 100ms epoch tick: workflows gain the `--wf-fuel-limit` runaway kill-switch (`runaway guest: exhausted compute budget`) | `accept_phase4.sh` |
| v2.6 | **Content-addressed provider registry** ([PROVIDERS.md](PROVIDERS.md), no WIT change): providers upload via `POST /api/providers?name=&tier=` (per-tier pre-flight at the door), stored as immutable sha256-keyed blobs with name→(tier,hash) bindings; live swap (next call uses it, replay returns recorded rows), rebind-by-hash rollback without re-shipping bytes, DELETE unbind, `/providers` UI page. Boot flags now UPSERT into the registry — flag providers persist across restarts | `smoke_provider_registry.sh` |
| v2.5 | **Effectful providers** ([PROVIDERS.md](PROVIDERS.md), `keel:provider@0.2.0` — guest WIT untouched, no guest rebuilds): `--provider-effectful` grants a provider the `host-http` import; every provider wire call is journaled at its own seq (`provider-http:<name>`) inside the provider-call scope, so a crash mid-provider re-fires only the truly in-flight call, with the same idempotency key. Pure tier's import-free guarantee unchanged (and enforced against effectful components). Per-tenant `providers_effectful` in fleets | `smoke_providers_effectful.sh` |

## Answered design questions

- **Multi-tenancy** is cells (process + db + token per tenant), never tenant
  columns in a shared database — one writer lock and one blast radius per
  tenant, not per fleet. See VISION.md for the full argument.
- **RLS** (row-level security): unnecessary under cell tenancy — the process
  boundary is the row boundary. A tenant's SQLite file contains only that
  tenant. If in-db multi-tenancy is ever revisited (it shouldn't be), RLS-style
  scoping would be the minimum bar; cells make the whole class moot.
- **Replication**: restore points are built in (online snapshots above);
  continuous off-box replication is deliberately delegated to
  [Litestream](https://litestream.io) (`docs/operations.md`) rather than
  reimplemented — SQLite WAL streaming is its whole job. Native streaming
  replication only becomes Keel's problem if a managed cloud gets built.

## Next

One WIT bump per stage, never two. The micro-cloud extension
([SPEC-MICROCLOUD.md](SPEC-MICROCLOUD.md)) is COMPLETE — v2.7 → v2.8 → v3.0 —
and its stretch item "per-route rate limits off the ledger" shipped as v3.1
(Amendment 1). Everything from here is demand-driven; the refined sequence
lives in status.md §R:

- **v3.5 "functions grow up"** — Amendment 2 (spec first): platform-api
  0.8.0 adds `config-get` (per-route config/secrets — the API-key unlock)
  and `kv-get`/`kv-set` (durable per-ref state, hard caps). One WIT bump,
  pure keel, no new dependencies.
- **v4.0 "the ecosystem release"** — Amendment 3: `wasi:http/proxy`
  compatibility mode + `wasi:keyvalue`, host surface via
  wasmtime-wasi(-http) — unmodified Spin/JCO components deploy on a
  one-binary cloud. Phase-sized.
- **host-kv for providers** — durable provider-scoped state (needs a key
  namespacing design; see PROVIDERS.md "Future").
- **Native streaming replication** — only if a managed cloud gets built;
  Litestream covers off-box replication until then.

### Cloud (unversioned, gated on adoption)

The hosted control plane on fleet cells — provisioning API, metering from
`/metrics`, billing last. Built if and when the open engine earns real users
(VISION.md). Nothing in v2.1–v2.4 blocks on it; everything in it builds on
v2.1–v2.4.
