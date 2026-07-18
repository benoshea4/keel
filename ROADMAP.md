# Keel roadmap

Positioning and the reasoning behind this sequence live in [VISION.md](VISION.md).
Every shipped item below is gated by a script under `scripts/` (they are the
definition of done) and runs in CI.

## Shipped

| Version | What | Proof |
|---|---|---|
| v1.0 | Durable core: journal-before-return, replay recovery, durable timers, external events, checkpoints + pruning, live v1→v2 upgrade with pre-flight, cancel (parked + spinning guests via epoch interruption), htmx UI | `accept_phase{1,2,3}.sh`, `smoke_cancel.sh`, `cargo test` |
| v1.1 | Operator token auth (`--api-token`/`KEEL_API_TOKEN`, cookie-digest UI login), per-guest memory caps (`--max-guest-memory-mb`) | `smoke_auth.sh` |
| v1.2 | Effects: `http-request` (method/headers/body, non-2xx as data, opt-in retries), journaled per-workflow KV (`kv-set`/`kv-get`), interval schedules (`/api/schedules`) — WIT 0.4.0 | `smoke_effects.sh` |
| v1.3 | Operability: `GET /api/workflows` (paged, filtered), `/metrics` (Prometheus text), retention GC (`--retain-terminal-hours`), UI logout | `smoke_effects.sh` |
| v2 (slice) | **DR**: periodic online snapshots (`--backup-dir`/`--backup-interval-secs`/`--backup-keep`) + one-shot `keel backup`, restore-by-copy runbook. **Cell tenancy**: `keel fleet --config` — one process/db/token per tenant, supervised, crash-only restarts | `smoke_dr.sh`, `smoke_fleet.sh` |
| v2.1 | **Secrets**: `secret(name)` host call (WIT 0.5.0) — `--secrets-file`, journal stores name + salted sha256 only, replay verifies against the live file (rotation fails loudly), values redacted from journaled HTTP requests. **Cron schedules**: 6-field expressions (UTC, seconds resolution) + `PATCH /api/schedules/{id}` pause/resume. **Per-call `timeout-ms`** on http-request | `smoke_secrets.sh`, extended `smoke_effects.sh` |
| v2.2 | **Crate split**: `keel-core` library (open/recover, upload, start, inspect — the binary is one consumer); "embeddable" is now literal. **Capability providers** (WIT 0.6.0, [PROVIDERS.md](PROVIDERS.md)): import-free `keel:provider` components registered via `--provider name=path.wasm` (per-tenant in fleets), journaled as `custom:<name>:<kind>`, memory+CPU bounded, failures as data | `smoke_embedded.sh`, `smoke_providers.sh`, extended `smoke_fleet.sh` |
| v2.3 | **KV versioning**: append-only `(workflow_id, key, seq, value)`; reads take the highest version, upgrades discard tail versions with the journal tail (the documented kv-vs-upgrade caveat is CLOSED), checkpoints compact superseded versions. **Idempotency keys**: `keel-idempotency-key: <workflow_id>:<seq>` on every http-request — wire-only (old journals replay untouched), stable across replay/re-send, guest-overridable, opt-out via empty value. No WIT bump | `smoke_kv_upgrade.sh`, extended `smoke_effects.sh` |
| v2.4 | **Surface & scale polish**: schedules page (create/pause/resume/delete) + durable-KV section on the workflow page · linux-arm64 release binaries · OTel traces behind `--features otel` (span per workflow, child span per host call, OTLP/http; default binary unaffected) · `keel_active_permits` metric · deploy recipes (`docs/deploy/`: systemd, Docker/compose, single-replica k8s) | `load_test.sh` (200 workflows through a cap of 8: cap respected via the permit gauge, all complete, journals dense), extended `smoke_effects.sh` |
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

One WIT bump per stage, never two.

- **Micro-cloud phases 5–6** (in flight, per the extension spec): phase 5 =
  sandbox metering + the playground judge (AC/WA/TLE/MLE/RE verdicts, usage
  page) as v2.8; phase 6 = hosted full-stack apps (Rust→WASM frontend + backend
  function, one zip, one binary) as v3.0.

Demand-driven after that:

- **host-kv for providers** — durable provider-scoped state (needs a key
  namespacing design; see PROVIDERS.md "Future").
- **Native streaming replication** — only if a managed cloud gets built;
  Litestream covers off-box replication until then.

### Cloud (unversioned, gated on adoption)

The hosted control plane on fleet cells — provisioning API, metering from
`/metrics`, billing last. Built if and when the open engine earns real users
(VISION.md). Nothing in v2.1–v2.4 blocks on it; everything in it builds on
v2.1–v2.4.
