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

### v2.3 — journal semantics

- **KV versioning** — append-only `(workflow_id, key, seq, value)`; reads
  resolve latest-at-or-below the current seq; upgrade tail-discard drops
  rows above C. Closes the documented kv-vs-upgrade caveat (docs/guests.md).
  Compaction rides the retention GC.
  *Gate:* an upgrade smoke asserting kv rolls back with the tail.
- **Idempotency keys** — the engine injects
  `keel-idempotency-key: <workflow_id>:<seq>` on http-request (opt-out), so
  remotes can dedupe the at-least-once window; documented server-side pattern.
  *Gate:* stub that dedupes by key.

### v2.4 — surface & scale polish

Schedules and kv in the UI · linux-arm64 release binaries (public repo = free
arm runners) · OTel traces behind a cargo feature · systemd unit + compose/k8s
recipes in docs · `scripts/load_test.sh` (hundreds of concurrent workflows:
cap respected, all complete, journal integrity holds).

### Cloud (unversioned, gated on adoption)

The hosted control plane on fleet cells — provisioning API, metering from
`/metrics`, billing last. Built if and when the open engine earns real users
(VISION.md). Nothing in v2.1–v2.4 blocks on it; everything in it builds on
v2.1–v2.4.
