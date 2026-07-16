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

Sequencing rationale: **secrets before platform work** — today a workflow
calling an authenticated API must carry its token in the workflow input, which
the journal stores in plaintext. That blocks real adoption more than any
missing abstraction, so it goes first. One WIT bump per stage, never two.

### v2.1 — real-workflow depth

- **`secret(name)` host call** (WIT 0.5.0). Design, resolving the
  journals-must-not-store-secrets tension: values come from `--secrets-file`
  (KEY=VALUE, mode 0600), the call returns the live value, and the journal
  records only `{name}` → `{sha256(value)}`. Replay re-reads the live file and
  verifies the hash — a rotated secret fails replay *loudly* ("secret X
  changed mid-workflow; restore it or cancel") instead of silently diverging.
  Secret bytes never touch the database.
  *Gate:* `smoke_secrets.sh` — secret in an `http-request` header against the
  stub; kill -9 → replay works; rotate the file → replay fails with the
  message.
- **Cron expressions on schedules** — `cron` field (seconds-resolution parser)
  as an alternative to `interval_ms`, same single-txn fire; plus
  `PATCH /api/schedules/{id}` for enable/disable.
  *Gate:* extend `smoke_effects.sh`.
- **Per-call `timeout-ms` on http-request** — rides the same 0.5.0 bump.

### v2.2 — platform

- **Crate split**: `keel-core` library (open engine, create/cancel/upgrade
  workflows, in-process API) + the `keel` binary consuming it — the
  "embeddable" in the positioning becomes literal.
  *Gate:* `examples/embedded.rs` runs a workflow in-process under `cargo test`.
- **Capability providers** — design doc (`PROVIDERS.md`) first, then: a
  provider is a wasm component implementing a `keel:provider` world
  (`handle(kind, request-json) → result<json>`); the engine journals
  `custom:<kind>` around it; registered via `--provider name=path.wasm`.
  Providers get no ambient capabilities — effects flow back through the
  engine. This is how Keel grows effects the way Envoy grows filters.
  *Gate:* a sample provider + `smoke_providers.sh`.

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
